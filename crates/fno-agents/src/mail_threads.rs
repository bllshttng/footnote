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
use std::io::{Read, Write};
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

/// The key a registry row answers to: its fno_id when one stands, else the
/// session id (law d-e952ed19: a name is a label, the id is the key).
fn registry_key(row: &Value) -> Option<String> {
    row.get("fno_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| row.get("harness_session_id").and_then(Value::as_str))
        .or_else(|| row.get("session_id").and_then(Value::as_str))
        .map(str::to_string)
}

/// The resolved key for a mail address: its registry row's key, else - when
/// the address is a UNIQUE prefix of exactly one row's id (a short id pasted
/// from a transcript) - that row's key, else the raw string. Resolving to the
/// fno_id is what keeps one agent one participant: rows that carry the id and
/// rows that carry the name join here, never split.
fn resolve_key(key: &str, registry: &[Value]) -> String {
    if key.is_empty() {
        return key.to_string();
    }
    if let Some(row) = registry_lookup(registry, key) {
        return registry_key(row).unwrap_or_else(|| key.to_string());
    }
    if key.len() >= 6 {
        let hits: Vec<&Value> = registry
            .iter()
            .filter(|row| {
                ["fno_id", "harness_session_id", "session_id"]
                    .iter()
                    .filter_map(|f| row.get(*f).and_then(Value::as_str))
                    .any(|id| id.starts_with(key))
            })
            .collect();
        if let [row] = hits[..] {
            if let Some(k) = registry_key(row) {
                return k;
            }
        }
    }
    key.to_string()
}

/// A system sender keys by its canonical `fno/<arm>` name, never by a
/// registry row, so a legacy stamp and its new name join as one voice.
fn participant_key(from_session: &str, name: &str, registry: &[Value]) -> String {
    if !from_session.is_empty() {
        return resolve_key(from_session, registry);
    }
    if crate::system_sender::is_system_sender(name) {
        return crate::system_sender::canonical(name).to_string();
    }
    resolve_key(name, registry)
}

/// The addressee's key: the registry row of the addressed name, else the raw
/// string (a project address or a name no registry row ever held becomes its
/// own participant, the judge disposition).
fn recipient_key(to: &str, registry: &[Value]) -> String {
    resolve_key(to, registry)
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
    match expires_at(line) {
        Some(e) => crate::state::rfc3339_like_to_secs(e).is_none_or(|t| t > now),
        None => true,
    }
}

fn expires_at(line: &Value) -> Option<&str> {
    line.get("meta")
        .and_then(|meta| meta.get("expires_at"))
        .and_then(Value::as_str)
        .or_else(|| line.get("expires").and_then(Value::as_str))
}

/// The projection of one chats store. `now` decides which announcements
/// still stand.
pub(crate) fn project_at(chats: &Path, registry: &[Value], now: u64) -> Value {
    let mut participants: BTreeMap<String, Value> = BTreeMap::new();
    // Counterparty scope counts and latest row per ended participant (R3).
    let mut scope_votes: BTreeMap<String, BTreeMap<String, (usize, String)>> = BTreeMap::new();
    // Each participant's most recent row ts (item 8's sort key).
    let mut last_seen: BTreeMap<String, String> = BTreeMap::new();
    // Pair rows keyed by the resolved, sorted participant pair; the threads
    // themselves are built once, after the walk, so both store dirs of an
    // exchange merge into one conversation.
    let mut pair_rows: BTreeMap<String, Vec<Value>> = BTreeMap::new();
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
        if entry.file_name().into_string().is_err() {
            continue;
        }
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
            // A `fleet:` address is a broadcast group, never an agent (the
            // operator's 2026-10-05 screenshots): it names no participant and
            // holds no last-seen vote.
            let to_is_broadcast = to.starts_with("fleet:");
            note(&mut participants, &from_key, reg_from, Some(from));
            if !to_is_broadcast {
                note(&mut participants, &to_key, reg_to, None);
            }
            let ts = v.get("ts").and_then(Value::as_str).unwrap_or("");
            let seen_keys: Vec<&String> = if to_is_broadcast {
                vec![&from_key]
            } else {
                vec![&from_key, &to_key]
            };
            for key in seen_keys {
                match last_seen.get_mut(key) {
                    Some(seen) if seen.as_str() >= ts => {}
                    _ => {
                        last_seen.insert(key.clone(), ts.to_string());
                    }
                }
            }
            let system_row = crate::system_sender::is_system_sender(from);
            let raw_body = v.get("body").and_then(Value::as_str).unwrap_or("");
            let body = crate::mail_header::display_body(raw_body);
            let mut row = json!({
                "id": v.get("id").and_then(Value::as_str).unwrap_or(""),
                "ts": v.get("ts").and_then(Value::as_str).unwrap_or(""),
                "from": crate::system_sender::canonical(from),
                "from_key": from_key,
                "to_key": to_key,
                "to": to,
                "summary": crate::mail_header::summary_of(&body),
                "body": body,
                "expires": expires_at(&v),
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
        pair_rows.entry(keys.join("\u{1}")).or_default().extend(rows);
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
        } else {
            // No scope ever named this participant: an ended sender with no
            // registry row (a leaked test fixture) or a dead agent whose scope
            // is gone reads as Archive, never as live-list pollution. The
            // operator's own journal voice stays out of the bucket.
            let live = p.get("live").and_then(Value::as_bool).unwrap_or(false);
            if !live && key != "user" {
                if let Some(obj) = p.as_object_mut() {
                    obj.insert("archive_scope".into(), json!("unresolved"));
                }
            }
        }
        if let Some(ts) = last_seen.get(key) {
            if let Some(obj) = p.as_object_mut() {
                obj.insert("last_ts".into(), json!(ts));
            }
        }
    }
    // One conversation per unordered agent pair: rows the store holds in
    // separate per-direction dirs interleave here by ts, and the chat id keys
    // off the RESOLVED pair (fno_id on both sides), never the dir name, so
    // read marks and selections survive the merge.
    let mut threads: Vec<Value> = pair_rows
        .into_iter()
        .map(|(pair, mut rows)| {
            rows.sort_by(|a, b| {
                a.get("ts")
                    .and_then(Value::as_str)
                    .cmp(&b.get("ts").and_then(Value::as_str))
            });
            let keys: Vec<String> = pair.split('\u{1}').map(str::to_string).collect();
            let first = keys.first().cloned().unwrap_or_default();
            let second = keys.get(1).cloned().unwrap_or_else(|| first.clone());
            let last_ts = rows
                .last()
                .and_then(|r| r.get("ts"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            json!({
                "chat_id": crate::chats::chat_id_for_pair(&first, &second),
                "participants": keys,
                "rows": rows,
                "last_ts": last_ts,
            })
        })
        .collect();
    threads.sort_by(|a, b| {
        a.get("last_ts")
            .and_then(Value::as_str)
            .cmp(&b.get("last_ts").and_then(Value::as_str))
    });
    json!({
        "participants": participants.values().cloned().collect::<Vec<_>>(),
        "threads": threads,
        "system": system,
        "channels": channels.into_iter().map(|(scope, rows)| json!({"scope": scope, "rows": rows})).collect::<Vec<_>>(),
        "announcements": announcements,
        "unreadable": unreadable,
    })
}

/// One participant row: the registry join over `key`. `extra` is the
/// message's own `from` name, the display fallback when no registry row
/// ever names the key; the id stays the key (d-6e0bf89b).
fn participant_row(key: &str, reg: Option<&Value>, extra: Option<&str>) -> Value {
    json!({
        "key": key,
        "name": reg
            .and_then(|r| r.get("name")).and_then(Value::as_str)
            .filter(|_| !crate::system_sender::is_system_sender(key))
            .or_else(|| extra
                .filter(|n| !n.is_empty() && !crate::system_sender::is_system_sender(key)))
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
}

/// One participant row, inserted once per key with its registry join. A
/// later sighting that carries a registry row upgrades a raw-key name, so
/// a session that registers after its first mail shows its name, never the
/// id.
fn note(
    participants: &mut BTreeMap<String, Value>,
    key: &str,
    row: Option<&Value>,
    extra: Option<&str>,
) {
    participants
        .entry(key.to_string())
        .and_modify(|v| {
            // A from-string name is a fallback: once the registry names the
            // participant, the stored row adopts its name. A row named by
            // key upgrades on any sighting; a fallback-named row only when
            // the registry actually supplies a name, so a nameless row
            // cannot clobber the fallback.
            let named = v.get("name").and_then(Value::as_str) != Some(key);
            let reg_name = row
                .and_then(|r| r.get("name"))
                .and_then(Value::as_str)
                .filter(|_| !crate::system_sender::is_system_sender(key));
            if row.is_some() && (!named || reg_name.is_some()) {
                *v = participant_row(key, row, None);
            }
        })
        .or_insert_with(|| participant_row(key, row, extra));
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
        "journal-reply" => run_journal_reply(&args[1..]),
        _ => {
            eprintln!("usage: fno-agents mail-threads [--format json | details --session <id> | journal-reply --to <name> --to-session <id> --in-reply-to <msg-id>]");
            2
        }
    }
}

fn run_journal_reply(args: &[String]) -> i32 {
    let value = |flag: &str| {
        args.iter()
            .position(|arg| arg == flag)
            .and_then(|index| args.get(index + 1))
            .map(String::as_str)
    };
    let (Some(to), Some(to_session), Some(parent)) =
        (value("--to"), value("--to-session"), value("--in-reply-to"))
    else {
        eprintln!("mail-threads journal-reply: --to, --to-session and --in-reply-to are required");
        return 2;
    };
    let mut body = String::new();
    if let Err(error) = std::io::stdin().read_to_string(&mut body) {
        eprintln!("mail-threads journal-reply: stdin: {error}");
        return 1;
    }
    let bus = journal_bus_path();
    match journal_reply_at(&bus, to, to_session, parent, &body) {
        Ok(id) => {
            println!("{id}");
            0
        }
        Err(error) => {
            eprintln!("mail-threads journal-reply: {error}");
            1
        }
    }
}

fn journal_bus_path() -> std::path::PathBuf {
    let home = AgentsHome::from_env();
    let dot_fno = home.root().parent().unwrap_or_else(|| home.root());
    crate::intel::bus_log_path(dot_fno)
}

fn journal_reply_at(
    bus: &Path,
    to: &str,
    to_session: &str,
    parent: &str,
    body: &str,
) -> Result<String, String> {
    let (chat, parent_row) = find_chat_for_message(&crate::chats::chats_dir(), parent)?;
    let thread = parent_row
        .get("thread")
        .and_then(Value::as_str)
        .filter(|thread| !thread.is_empty())
        .unwrap_or(parent);
    let id = crate::announce::new_msg_id();
    let reply = json!({
        "v": 1,
        "id": id,
        "ts": crate::announce::now_iso(),
        "thread": thread,
        "from": "user",
        "to": to,
        "kind": "send",
        "to_kind": "session",
        "delivery": "typed",
        "in_reply_to": parent,
        "meta": {"lane": "mux-reply", "to_session": to_session},
        "word_count": body.split_whitespace().count(),
        "body": body,
    });
    crate::announce::append_line(bus, &reply)?;
    append_projected_reply(
        &chat,
        &reply,
        parent_row.get("chat_id").and_then(Value::as_str),
    )
    .map_err(|error| format!("reply {id} is on the bus, but chat projection failed: {error}"))?;
    Ok(id)
}

fn find_chat_for_message(chats: &Path, id: &str) -> Result<(std::path::PathBuf, Value), String> {
    let entries = std::fs::read_dir(chats)
        .map_err(|error| format!("read chats {}: {error}", chats.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let file = path.join("messages.jsonl");
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for line in text.lines() {
            let Ok(row) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if row.get("id").and_then(Value::as_str) == Some(id) {
                return Ok((path, row));
            }
        }
    }
    Err(format!("parent message {id:?} has no chat projection"))
}

fn append_projected_reply(
    chat: &Path,
    reply: &Value,
    parent_chat_id: Option<&str>,
) -> Result<(), String> {
    let file = chat.join("messages.jsonl");
    let lock_path = std::path::PathBuf::from(format!("{}.lock", file.display()));
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| format!("chat lock open {}: {error}", lock_path.display()))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) => return Err(format!("chat lock: {error}")),
        }
    }
    let mut row = reply.clone();
    let object = row
        .as_object_mut()
        .ok_or_else(|| "reply is not an object".to_string())?;
    object.insert("type".into(), json!("message"));
    object.insert(
        "chat_id".into(),
        json!(parent_chat_id.unwrap_or_else(|| chat
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(""))),
    );
    let mut line = serde_json::to_string(&row).map_err(|error| format!("serialize: {error}"))?;
    line.push('\n');
    let mut output = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
        .map_err(|error| format!("chat file open {}: {error}", file.display()))?;
    output
        .write_all(line.as_bytes())
        .map_err(|error| format!("chat append: {error}"))
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
            json!({"name": "prefix-agent", "fno_id": "sess-f2571a54-full",
                   "harness_session_id": "sess-f2571a54-full",
                   "harness": "claude", "liveness": "alive", "status": "live"}),
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
        // AC4-HP: a session the registry never names still shows the
        // sender's own from name; the session id stays the key.
        write_chat(
            &chats,
            "chat-f0f0f0f0f0f0f0f0",
            &[json!({
                "type": "message", "kind": "send", "v": 1,
                "id": "fmail-d0d0d0d0d0d0", "ts": "2026-10-01T09:15:00Z",
                "thread": "fmail-d0d0d0d0d0d0",
                "from": "candor", "from_session": "s-lone", "to": "vellum",
                "body": "Pinging the shelf.",
            })],
        );
        // AC7-AC9-HP: old envelope mail and old header mail read as the
        // body at display time; a mid-text mention never rewrites.
        write_chat(
            &chats,
            "chat-e0e0e0e0e0e0e0e0",
            &[
                msg(
                    "fmail-b1b1b1b1b1b1",
                    "2026-10-01T09:16:00Z",
                    "s-candor",
                    "vellum",
                    "<fno_mail from=\"a\" id=\"fmail-b1b1b1b1b1b1\">Ship it.</fno_mail>",
                ),
                msg(
                    "fmail-b2b2b2b2b2b2",
                    "2026-10-01T09:17:00Z",
                    "s-candor",
                    "vellum",
                    "`@a \u{b7} fmail-b2b2b2b2b2b2 \u{b7} Ship it.`\nShip it. Then merge.",
                ),
                msg(
                    "fmail-b3b3b3b3b3b3",
                    "2026-10-01T09:18:00Z",
                    "s-candor",
                    "vellum",
                    "see <fno_mail> docs",
                ),
            ],
        );
        // The exchange's OTHER half, stored in its own per-direction dir:
        // the projection merges the pair into ONE interleaved conversation.
        write_chat(
            &chats,
            "chat-mmmmmmmmmmmmmmmm",
            &[json!({
                "type": "message", "kind": "send", "v": 1,
                "id": "fmail-222222222223", "ts": "2026-10-01T09:02:00Z", "thread": "fmail-222222222223",
                "from": "s-vellum", "from_session": "s-vellum", "to": "s-candor",
                "body": "Halfway there.",
            })],
        );
        // A bare short session id resolves to its one registry row: the
        // participant keys by the full fno_id and shows the row's name.
        write_chat(
            &chats,
            "chat-nnnnnnnnnnnnnnnn",
            &[msg(
                "fmail-c1c1c1c1c1c1",
                "2026-10-01T09:19:00Z",
                "f2571a54",
                "vellum",
                "Short id ping.",
            )],
        );
        // An unresolvable leaked fixture reads as Archive, not live-list.
        write_chat(
            &chats,
            "chat-oooooooooooooooo",
            &[msg(
                "fmail-d1d1d1d1d1d1",
                "2026-10-01T09:21:00Z",
                "lead-a",
                "vellum",
                "Fixture leak.",
            )],
        );
        // A pair-kind row addressed `fleet:all` names no fleet participant.
        write_chat(
            &chats,
            "chat-pppppppppppppppp",
            &[msg(
                "fmail-e1e1e1e1e1e1",
                "2026-10-01T09:22:00Z",
                "s-candor",
                "fleet:all",
                "Straight to the group.",
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
                    "from": "fno/fleet-incident", "to": "fleet:fno",
                    "meta": {"scope": "fno", "expires_at": "2026-10-03T12:00:00Z"},
                    "body": "Heavy benchmark running.",
                }),
                json!({
                    "type": "message", "kind": "announce", "v": 1,
                    "id": "fmail-999999999999", "ts": "2026-10-01T09:01:30Z",
                    "from": "fno/fleet-incident", "to": "fleet:fno",
                    "meta": {"scope": "fno", "expires_at": "2026-10-01T10:00:00Z"},
                    "body": "expired incident",
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
        // a+m merge into one pair thread; n, o, p each stand alone; the
        // system and announce dirs contribute no pair thread.
        assert_eq!(threads.len(), 7, "pair chats only: {threads:?}");
        // Old mail: an unmigrated legacy row shows the raw tag (the prompt
        // to run the one-time migration), the pasted header-summary repeat
        // strips once, mid-text mention untouched (AC7-AC9-HP).
        let body_of = |id: &str| -> String {
            threads
                .iter()
                .flat_map(|t| t.get("rows").and_then(Value::as_array).unwrap())
                .find(|r| r.get("id").and_then(Value::as_str) == Some(id))
                .map(|r| {
                    r.get("body")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                })
                .unwrap_or_default()
        };
        // Display strips the legacy paired envelope at render (the store
        // still holds it until `chats migrate --envelopes` runs).
        assert_eq!(body_of("fmail-b1b1b1b1b1b1"), "Ship it.");
        assert_eq!(body_of("fmail-b2b2b2b2b2b2"), "Ship it. Then merge.");
        assert_eq!(body_of("fmail-b3b3b3b3b3b3"), "see <fno_mail> docs");
        // AC4-HP: the registry never named s-lone; its from name shows and
        // the id stays the key.
        let lone = participants
            .iter()
            .find(|p| p.get("key").and_then(Value::as_str) == Some("s-lone"))
            .expect("lone participant");
        assert_eq!(lone.get("name").and_then(Value::as_str), Some("candor"));
        assert_eq!(lone.get("key").and_then(Value::as_str), Some("s-lone"));
        // AC3-HP: a later registry sighting upgrades a raw-key name; the
        // id stays the key.
        let mut named = BTreeMap::new();
        let quill_row = json!({"name": "quill", "fno_id": "s1", "harness_session_id": "s1"});
        note(&mut named, "s1", None, Some("s1"));
        assert_eq!(named["s1"]["name"], "s1");
        note(&mut named, "s1", Some(&quill_row), None);
        assert_eq!(named["s1"]["name"], "quill");
        assert_eq!(named["s1"]["key"], "s1");
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
        assert_eq!(
            ann[0].get("expires").and_then(Value::as_str),
            Some("2026-10-03T12:00:00Z")
        );
        // The hold digest never appears (R8) and the broken line counts (AC1-ERR).
        let bodies = serde_json::to_string(&projection).unwrap();
        assert!(!bodies.contains("digest line"), "hold dropped: {bodies}");
        assert_eq!(projection.get("unreadable"), Some(&json!(1)));

        // One chat per unordered pair: both stored halves interleave by ts
        // under a chat id keyed off the resolved pair.
        let merged = threads
            .iter()
            .find(|t| {
                t.get("chat_id").and_then(Value::as_str)
                    == Some(crate::chats::chat_id_for_pair("s-candor", "s-vellum").as_str())
            })
            .expect("the candor/vellum halves merge");
        let ids: Vec<&str> = merged
            .get("rows")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_str))
            .collect();
        assert_eq!(
            ids,
            ["fmail-111111111111", "fmail-222222222223", "fmail-222222222222"],
            "interleaved by ts: {ids:?}"
        );
        // The short id joins its registry row: one participant, named.
        let prefix_p = participants
            .iter()
            .find(|p| p.get("key").and_then(Value::as_str) == Some("sess-f2571a54-full"))
            .expect("short id resolves to its row");
        assert_eq!(
            prefix_p.get("name").and_then(Value::as_str),
            Some("prefix-agent")
        );
        assert!(
            !participants
                .iter()
                .any(|p| p.get("key").and_then(Value::as_str) == Some("f2571a54")),
            "the short id never stands alone"
        );
        // A fleet address is a group, not an agent.
        assert!(
            !participants
                .iter()
                .any(|p| p.get("key").and_then(Value::as_str) == Some("fleet:all")),
            "broadcast addresses never become participants"
        );
        // An unresolvable ended sender archives.
        let fixture = participants
            .iter()
            .find(|p| p.get("key").and_then(Value::as_str) == Some("lead-a"))
            .expect("the fixture participant exists");
        assert_eq!(
            fixture.get("archive_scope").and_then(Value::as_str),
            Some("unresolved")
        );

        let state = temp_root("journal-reply");
        let state_root = state.join(".fno");
        let agents_root = state_root.join("agents");
        std::fs::create_dir_all(&agents_root).unwrap();
        let prior_state = std::env::var_os("FNO_STATE_DIR");
        let prior_agents = std::env::var_os(crate::paths::HOME_ENV);
        let prior_bus_dir = std::env::var_os("FNO_BUS_DIR");
        std::env::set_var("FNO_STATE_DIR", &state_root);
        std::env::set_var(crate::paths::HOME_ENV, &agents_root);
        let custom_bus = state.join("custom-bus");
        std::env::set_var("FNO_BUS_DIR", &custom_bus);
        assert_eq!(
            journal_bus_path(),
            custom_bus.join("messages.jsonl"),
            "journal replies follow the configured live bus"
        );
        let bus = custom_bus.join("messages.jsonl");
        // The chat projection can outlive rotated bus segments; a reply keeps
        // the parent's original thread and uses the canonical meta address.
        let parent = json!({
            "v": 1, "id": "fmail-aaaaaaaaaaaa", "ts": "2026-10-02T11:00:00Z",
            "type": "message", "chat_id": "chat-aaaaaaaaaaaaaaaa",
            "thread": "fmail-root-thread", "from": "s-sender", "to": "receiver",
            "kind": "send", "body": "parent message",
        });
        let chats = crate::chats::chats_dir();
        write_chat(&chats, "chat-aaaaaaaaaaaaaaaa", &[parent.clone()]);
        assert!(journal_reply_at(
            &bus,
            "receiver",
            "s-receiver",
            "fmail-ffffffffffff",
            "re: cannot append",
        )
        .is_err());
        assert!(!bus.exists(), "unknown parent leaves the bus unchanged");

        let id = journal_reply_at(
            &bus,
            "receiver",
            "s-receiver",
            "fmail-aaaaaaaaaaaa",
            "re: user reply",
        )
        .unwrap();
        let bus_rows = crate::announce::read_bus_segments(&bus);
        let reply = bus_rows
            .iter()
            .find(|row| row.get("id").and_then(Value::as_str) == Some(id.as_str()))
            .unwrap();
        assert_eq!(reply["from"], "user");
        assert_eq!(reply["to"], "receiver");
        assert_eq!(reply["meta"]["to_session"], "s-receiver");
        assert_eq!(reply["kind"], "send");
        assert_eq!(reply["to_kind"], "session");
        assert_eq!(reply["delivery"], "typed");
        assert_eq!(reply["in_reply_to"], "fmail-aaaaaaaaaaaa");
        assert_eq!(reply["thread"], "fmail-root-thread");
        assert_eq!(reply["meta"]["lane"], "mux-reply");
        assert_eq!(reply["word_count"], 3);
        assert_eq!(reply["body"], "re: user reply");
        let journaled = project_at(
            &crate::chats::chats_dir(),
            &[],
            crate::state::rfc3339_like_to_secs("2026-10-02T12:00:00Z").unwrap(),
        );
        let projected = journaled["threads"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|thread| thread["rows"].as_array().unwrap())
            .find(|row| row.get("id").and_then(Value::as_str) == Some(id.as_str()))
            .unwrap();
        assert_eq!(projected["from"], "user");
        assert_eq!(projected["in_reply_to"], "fmail-aaaaaaaaaaaa");
        if let Some(value) = prior_state {
            std::env::set_var("FNO_STATE_DIR", value);
        } else {
            std::env::remove_var("FNO_STATE_DIR");
        }
        if let Some(value) = prior_agents {
            std::env::set_var(crate::paths::HOME_ENV, value);
        } else {
            std::env::remove_var(crate::paths::HOME_ENV);
        }
        if let Some(value) = prior_bus_dir {
            std::env::set_var("FNO_BUS_DIR", value);
        } else {
            std::env::remove_var("FNO_BUS_DIR");
        }
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&state);
    }
}
