//! `fno-agents backlog-note` (wave 2): the native note action the
//! Python `fno backlog note` bridge calls. The Rust side owns the note
//! feed (every note appends a comment row to the node's thread,
//! stamped with the writer's identity), the `--clear` state route, and
//! history routing (machine, wave); the bridge keeps the shipped recipient
//! walk (`note_notify`, its test contract), evidence checks, identity,
//! archived refusal, and the mail transport.
use crate::backlog::model::Node;
use crate::backlog::node_state::{self, StateError};
use crate::backlog::note_history;
use crate::graph_store::{self};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::graph_get::default_graph_path;

/// The note-kind feed vocabulary. These are thread rows, not asks:
/// no open state, no reply threading.
const NOTE_KINDS: [&str; 4] = ["progress", "finding", "ruling", "collision"];

/// One parsed invocation of the note action.
struct NoteArgs {
    node: String,
    body: Option<String>,
    body_file: Option<String>,
    stdin: bool,
    read: bool,
    clear: bool,
    replace: bool,
    quiet: bool,
    json_out: bool,
    if_revision: Option<u64>,
    machine: Option<String>,
    wave: bool,
    refresh_marker: bool,
    graph: Option<PathBuf>,
    self_session: Option<String>,
    reads: Option<String>,
    import_record: Option<String>,
    blocking: bool,
    resolve: Option<String>,
    block_cmd: Option<String>,
    block_excerpt_file: Option<String>,
    kind: Option<String>,
}

fn parse_args(args: &[String]) -> Result<NoteArgs, String> {
    let mut out = NoteArgs {
        node: String::new(),
        body: None,
        body_file: None,
        stdin: false,
        read: false,
        clear: false,
        replace: false,
        quiet: false,
        json_out: false,
        if_revision: None,
        machine: None,
        wave: false,
        refresh_marker: false,
        graph: None,
        self_session: None,
        reads: None,
        import_record: None,
        blocking: false,
        resolve: None,
        block_cmd: None,
        block_excerpt_file: None,
        kind: None,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--graph" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| "--graph needs a path".to_string())?;
                out.graph = Some(PathBuf::from(v));
            }
            "--body-file" => {
                i += 1;
                out.body_file = Some(
                    args.get(i)
                        .ok_or_else(|| "--body-file needs a path".to_string())?
                        .clone(),
                );
            }
            "--stdin" => out.stdin = true,
            "--read" => out.read = true,
            "--clear" => out.clear = true,
            "--replace" => out.replace = true,
            "--quiet" => out.quiet = true,
            "--json" | "-J" => out.json_out = true,
            "--if-revision" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| "--if-revision needs a number".to_string())?;
                out.if_revision = Some(v.parse().map_err(|_| format!("bad revision: {v}"))?);
            }
            "--machine" => {
                i += 1;
                out.machine = Some(
                    args.get(i)
                        .ok_or_else(|| "--machine needs a kind".to_string())?
                        .clone(),
                );
            }
            "--wave" => out.wave = true,
            "--refresh-marker" => out.refresh_marker = true,
            "--node" => {
                i += 1;
                out.node = args
                    .get(i)
                    .ok_or_else(|| "--node needs an id".to_string())?
                    .clone();
            }
            "--reads" => {
                i += 1;
                out.reads = args
                    .get(i)
                    .ok_or_else(|| "--reads needs JSON".to_string())?
                    .clone()
                    .into();
            }
            "--blocking" => out.blocking = true,
            "--import-record" => {
                i += 1;
                out.import_record = Some(
                    args.get(i)
                        .ok_or_else(|| "--import-record needs a path or -".to_string())?
                        .clone(),
                );
            }
            "--resolve" => {
                i += 1;
                out.resolve = Some(
                    args.get(i)
                        .ok_or_else(|| "--resolve needs a finding id".to_string())?
                        .clone(),
                );
            }
            "--block-cmd" => {
                i += 1;
                out.block_cmd = Some(
                    args.get(i)
                        .ok_or_else(|| "--block-cmd needs text".to_string())?
                        .clone(),
                );
            }
            "--block-excerpt-file" => {
                i += 1;
                out.block_excerpt_file = Some(
                    args.get(i)
                        .ok_or_else(|| "--block-excerpt-file needs a path or -".to_string())?
                        .clone(),
                );
            }
            "--self-session" => {
                i += 1;
                out.self_session = Some(
                    args.get(i)
                        .ok_or_else(|| "--self-session needs an id".to_string())?
                        .clone(),
                );
            }
            "--kind" => {
                i += 1;
                out.kind = Some(
                    args.get(i)
                        .ok_or_else(|| "--kind needs a kind".to_string())?
                        .clone(),
                );
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown flag {other}"));
            }
            other => {
                if out.node.is_empty() {
                    out.node = other.to_string();
                } else if out.body.is_none() {
                    out.body = Some(other.to_string());
                }
            }
        }
        i += 1;
    }
    Ok(out)
}

/// The main entry: parse, load, route, refuse-or-write, print the
/// JSON receipt on stdout. Exit 2 usage, 1 missing graph/node, 3 a refusal
/// that wrote nothing (the cross-session guard, or a stale --if-revision;
/// the bridge adds the nobody-bound walk on this code), 5 graph read
/// failed, 0 written.
pub fn run_note(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno-agents backlog-note: {e}");
            return 2;
        }
    };
    let graph = parsed.graph.clone().unwrap_or_else(default_graph_path);
    // Finding routes. `--resolve` needs no node and no body; `--blocking`
    // is routed after entry resolution below. Neither touches current state.
    if let Some(finding_id) = parsed.resolve.clone() {
        return run_finding_resolve(&parsed, &graph, &finding_id);
    }
    // The import route: restore one captured node record. Like `--resolve`
    // it needs no node argument and reads no body.
    if let Some(source) = parsed.import_record.clone() {
        return run_import_record(&parsed, &graph, &source);
    }
    let body = match read_body(&parsed) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("fno-agents backlog-note: {e}");
            return 1;
        }
    };
    // Backend-aware read: entry resolution must see post-flip nodes,
    // which exist only in graph.db; the frozen json keeper does not know
    // them. read_rows switches on graph_meta.backend.
    let entries = match graph_store::read_rows(&graph) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("fno-agents backlog-note: graph read failed: {e}");
            return 5;
        }
    };
    let entry = crate::graph_get::find_entry(&entries, &parsed.node);
    let Some(entry) = entry else {
        eprintln!("Error: no node resolves to '{}'", parsed.node);
        return 1;
    };
    let node_id = entry
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // Machine and wave producers: history-only, no recipients, no state.
    if parsed.blocking {
        let Some(body) = body else {
            eprintln!(
                "fno-agents backlog-note: a blocking finding needs a body (positional, --body-file, or --stdin)"
            );
            return 2;
        };
        return run_finding_create(&parsed, &graph, &node_id, body);
    }
    if parsed.machine.is_some() || parsed.wave {
        return run_machine(&parsed, &graph, &node_id, body);
    }
    write_human(&parsed, &graph, entry, body)
}

/// One note body: positional, `--body-file`, or `--stdin`, exactly one.
fn read_body(parsed: &NoteArgs) -> Result<Option<String>, String> {
    if parsed.stdin {
        use std::io::Read;
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| format!("stdin read failed: {e}"))?;
        return Ok(Some(s));
    }
    if let Some(f) = &parsed.body_file {
        let s = std::fs::read_to_string(f).map_err(|e| format!("body file read failed: {e}"))?;
        return Ok(Some(s));
    }
    Ok(parsed.body.clone())
}

/// Machine `task_done`/`run_summary` records and structured wave additions:
/// history only, never the human current state; a wave leaves the bounded
/// `state_needs_refresh` marker.
fn run_machine(
    parsed: &NoteArgs,
    graph: &std::path::Path,
    node_id: &str,
    body: Option<String>,
) -> i32 {
    let Some(body) = body else {
        eprintln!("fno-agents backlog-note: machine/wave records need a body");
        return 2;
    };
    let body = node_state::normalize_prose(&body);
    if body.chars().count() > node_state::PROSE_LIMIT {
        eprintln!(
            "fno-agents backlog-note: history-only prose is capped at {} characters",
            node_state::PROSE_LIMIT
        );
        return 1;
    }
    let rev = node_state::current_revision(graph, node_id).unwrap_or(0);
    // Both machine kinds are progress records; a wave additionally sets the
    // refresh marker below.
    let reason = note_history::REASON_MACHINE_RECORD;
    if let Err(e) = note_history::append(
        graph,
        node_id,
        reason,
        Some(rev),
        None,
        &json!({ "body": body, "kind": parsed.machine }),
        parsed.self_session.as_deref(),
        None,
    ) {
        eprintln!("fno-agents backlog-note: history write failed: {e}");
        return 1;
    }
    if parsed.wave {
        if let Err(e) = set_refresh_marker(graph, node_id) {
            eprintln!("fno-agents backlog-note: refresh marker write failed: {e}");
            return 1;
        }
    }
    if parsed.json_out {
        crate::backlog::receipt::emit_line(
            &json!({
                "status": "ok",
                "routed": "history",
                "node_id": node_id,
                "revision": rev,
            })
            .to_string(),
        );
    } else {
        crate::backlog::receipt::emit_line(&format!("recorded {node_id}: history"));
    }
    0
}

/// The --resolve route: stamp resolved_at through the findings API and emit
/// the telemetry event after commit. Exit 0 resolved, 1 unknown id or store
/// error, 2 usage.
fn run_finding_resolve(parsed: &NoteArgs, graph: &std::path::Path, finding_id: &str) -> i32 {
    if parsed.blocking {
        eprintln!("fno-agents backlog-note: --blocking and --resolve are separate routes");
        return 2;
    }
    let receipt = crate::backlog::api::finding_resolve(
        &crate::backlog::api::Store::new(graph),
        finding_id,
        parsed.self_session.as_deref(),
    );
    match receipt {
        Ok(r) => {
            emit_finding_event(
                "review_finding_resolved",
                json!({ "finding_id": r.finding_id, "status": r.status }),
            );
            let out = json!({
                "status": "ok", "routed": "resolve", "finding_id": r.finding_id,
                "resolve_status": r.status, "resolved_at": r.resolved_at, "version": r.version,
                "line": format!("resolved {}: {} (at {})", r.finding_id, r.status, r.resolved_at),
            });
            emit_human(parsed.json_out, &out);
            0
        }
        Err(e) => {
            eprintln!("fno-agents backlog-note: {}", e.0);
            1
        }
    }
}

/// The --blocking route: create the finding through the findings API, emit
/// the telemetry event after commit, print the receipt. Exit 0 written,
/// 1 refused or failed, 2 usage.
fn run_finding_create(
    parsed: &NoteArgs,
    graph: &std::path::Path,
    node_id: &str,
    body: String,
) -> i32 {
    let excerpt = read_excerpt(&parsed.block_excerpt_file);
    let excerpt = match excerpt {
        Ok(e) => e,
        Err(e) => {
            eprintln!("fno-agents backlog-note: {e}");
            return 1;
        }
    };
    let receipt = crate::backlog::api::finding_create(
        &crate::backlog::api::Store::new(graph),
        node_id,
        crate::backlog::api::FindingInput {
            body,
            block_cmd: parsed.block_cmd.clone(),
            block_excerpt: excerpt,
            source_session_id: parsed.self_session.clone(),
            source_harness: None,
        },
    );
    match receipt {
        Ok(r) => {
            emit_finding_event(
                "review_finding",
                json!({ "finding_id": r.finding_id, "node_id": r.node_id }),
            );
            let pointer = finding_pointer_line(&r.node_id, &r.finding_id);
            let delivery = notice_holder(&r.node_id, &pointer);
            let line = match &delivery {
                Some(note) => format!("recorded {} on {}; {note}", r.finding_id, r.node_id),
                None => format!(
                    "recorded {} on {}; no live reader, it gates the next worker",
                    r.finding_id, r.node_id
                ),
            };
            let out = json!({
                "status": "ok", "routed": "finding", "finding_id": r.finding_id,
                "node_id": r.node_id, "version": r.version,
                "delivery": delivery, "pointer": pointer, "line": line,
            });
            emit_human(parsed.json_out, &out);
            0
        }
        Err(e) => {
            eprintln!("fno-agents backlog-note: {}", e.0);
            1
        }
    }
}

/// The finding pointer: node, id, read and resolve commands - the whole
/// delivered body, never the finding text.
fn finding_pointer_line(node_id: &str, finding_id: &str) -> String {
    format!(
        "finding {finding_id} on {node_id}: blocking. \
         Read: fno backlog notes findings {node_id}. \
         Clear: fno backlog note --resolve {finding_id}"
    )
}

/// Best-effort holder notice after a finding lands: one pointer line to the
/// live claim holder, injected detached through this binary's own
/// mail-inject lane. A miss only degrades the receipt line; the durable
/// record gates regardless.
pub(crate) fn notice_holder(node_id: &str, pointer: &str) -> Option<String> {
    let (state, record) = crate::claims::status(&format!("node:{node_id}"), None);
    if !matches!(
        state,
        crate::claims::ClaimState::Live | crate::claims::ClaimState::Suspect
    ) {
        return None;
    }
    let record = record?;
    let sid = record.holder.rsplit(':').next()?.to_string();
    let harness = record.harness.clone().unwrap_or_else(|| "claude".into());
    let mut child = std::process::Command::new(std::env::current_exe().ok()?)
        .args([
            "mail-inject",
            "--session",
            &sid,
            "--harness",
            &harness,
            "--sender",
            "note",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    use std::io::Write;
    let _ = child
        .stdin
        .take()
        .and_then(|mut stdin| stdin.write_all(pointer.as_bytes()).ok());
    Some(format!("pointer sent to {}", record.holder))
}

/// `note comment` - the user-to-agent thread on a node. Forms:
///   `<id> "<text>" [--author user|agent]` posts a comment (state open);
///   `<id> --reply <cid> [--state accepted|done|declined] [--ref R] "<text>"`
///   replies, moving the thread head; `<id> --list` renders the thread
///   oldest first; `--open` lists every open ask board-wide. The write goes
///   through `api::comment_create`; a live claim holder gets the reply
///   pointer through `notice_holder`.
pub fn run_comment(args: &[String]) -> i32 {
    let mut graph: Option<PathBuf> = None;
    let mut json_out = false;
    let mut open_only = false;
    let mut list = false;
    let mut reply: Option<String> = None;
    let mut state: Option<String> = None;
    let mut state_ref: Option<String> = None;
    let mut author: Option<String> = None;
    let mut self_session: Option<String> = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--graph" => graph = it.next().map(PathBuf::from),
            "--json" | "-J" => json_out = true,
            "--open" => open_only = true,
            "--list" => list = true,
            "--reply" => reply = it.next().cloned(),
            "--state" => state = it.next().cloned(),
            "--ref" => state_ref = it.next().cloned(),
            "--author" => author = it.next().cloned(),
            "--self-session" => self_session = it.next().cloned(),
            other => positionals.push(other.to_string()),
        }
    }
    let graph = graph.unwrap_or_else(default_graph_path);
    let store = super::api::Store::new(&graph);

    // `--open`: every open ask across the board (node, comment id, age, text).
    if open_only {
        let rows = match graph_store::read_rows(&graph) {
            Ok(rows) => rows,
            Err(e) => {
                eprintln!("fno-agents backlog-note comment: graph read failed: {e}");
                return 5;
            }
        };
        let mut found = 0;
        for row in &rows {
            let Some(node_id) = crate::graph_store::entry_id(row) else {
                continue;
            };
            for note in row
                .get("progress_notes")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[])
            {
                let kind = note.get("kind").and_then(Value::as_str);
                // Extras flatten to the row's top level in storage.
                let state = note.get("state").and_then(Value::as_str);
                if kind != Some("comment") || state != Some("open") {
                    continue;
                }
                let cid = note
                    .get("comment_id")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let author = note.get("author").and_then(Value::as_str).unwrap_or("?");
                let created = note.get("ts").and_then(Value::as_str).unwrap_or("");
                let body = note.get("text").and_then(Value::as_str).unwrap_or("");
                println!(
                    "{node_id}  {cid}  {} {author}  {body}",
                    comment_age(created)
                );
                found += 1;
            }
        }
        if found == 0 {
            println!("no open asks");
        }
        return 0;
    }

    let Some(node_id) = positionals.first().cloned() else {
        eprintln!("fno-agents backlog-note comment: a node id (or --open) is required");
        return 2;
    };

    // `<id> --list`: the thread, oldest first.
    if list {
        let thread = match super::api::comments(&store, &node_id, &super::api::Page::default()) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("fno-agents backlog-note comment: {}", e.0);
                return 1;
            }
        };
        for row in &thread.nodes {
            let author = row
                .extras
                .get("author")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let state = row.extras.get("state").and_then(Value::as_str);
            let age = comment_age(row.created_at.as_deref().unwrap_or(""));
            let body = row.body.as_deref().unwrap_or("");
            let indent = if row.kind.as_deref() == Some("reply") {
                "  "
            } else {
                ""
            };
            let mark = match state {
                Some("open") => "○",
                Some("accepted") => "◐",
                Some("done") => "✓",
                Some("declined") => "✗",
                _ => " ",
            };
            println!("{indent}{mark} {author} · {age}  {body}");
            // Who wrote it, shown in the thread and copyable.
            let mut who: Vec<String> = Vec::new();
            if let Some(name) = row.extras.get("agent_name").and_then(Value::as_str) {
                who.push(name.to_string());
            }
            if let Some(model) = row.extras.get("model").and_then(Value::as_str) {
                who.push(model.to_string());
            }
            if let Some(session) = &row.source_session_id {
                who.push(format!("session {session}"));
            }
            if let Some(working) = row.extras.get("working_node").and_then(Value::as_str) {
                who.push(format!("on {working}"));
            }
            if !who.is_empty() {
                println!("{indent}      · {}", who.join(" · "));
            }
        }
        return 0;
    }

    let text = positionals[1..].join(" ");
    if text.trim().is_empty() {
        eprintln!("fno-agents backlog-note comment: nothing to post");
        return 2;
    }
    let kind = if reply.is_some() { "reply" } else { "comment" };
    // An agent comment carries who wrote it: the process-provable
    // session and harness, the worker name, and the node the session is
    // working. The observed model resolves inside the write from the row
    // it already loads, so no read happens here. A user comment carries
    // the user's own word; no stamp.
    let ident = if author.as_deref() != Some("user") {
        Some(comment_identity(self_session.as_deref()))
    } else {
        None
    };
    let input = super::api::CommentCreateInput {
        body: text.clone(),
        kind: Some(kind.to_string()),
        title: None,
        author,
        reply_to: reply.clone(),
        state,
        state_ref,
        session_id: ident.as_ref().and_then(|i| i.session_id.clone()),
        harness: ident.as_ref().and_then(|i| i.harness.clone()),
        agent_name: ident.as_ref().and_then(|i| i.agent_name.clone()),
        model: ident.as_ref().and_then(|i| i.model.clone()),
        working_node: ident.as_ref().and_then(|i| i.working_node.clone()),
        reads: None,
    };
    match super::api::comment_create(&store, &node_id, input) {
        Ok(payload) => {
            let minted = payload
                .node
                .as_ref()
                .and_then(|n| n.comments.as_ref())
                .and_then(|rows| rows.last())
                .and_then(|c| c.extras.get("comment_id").and_then(Value::as_str))
                .map(str::to_string);
            let holder = match (&reply, &minted) {
                (None, Some(cid)) => notice_holder(
                    &node_id,
                    &format!(
                        "comment {cid} on {node_id}: {}\nReply: fno backlog note comment {node_id} --reply {cid} --state accepted|done|declined",
                        first_line(&text)
                    ),
                ),
                _ => notice_holder(&node_id, &format!("reply on {node_id}: {}", first_line(&text))),
            };
            if json_out {
                let receipt = serde_json::json!({
                    "success": true,
                    "node": node_id,
                    "comment_id": minted,
                    "holder": holder,
                });
                println!("{receipt}");
            } else {
                println!("comment posted on {node_id}");
                match holder {
                    Some(line) => println!("{line}"),
                    None => println!("no live reader: the comment waits on the node"),
                }
            }
            0
        }
        Err(e) => {
            eprintln!("fno-agents backlog-note comment: {}", e.0);
            1
        }
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

/// Relative age for a thread row, from the row's RFC 3339 stamp.
fn comment_age(created_at: &str) -> String {
    use chrono::{DateTime, Utc};
    let Ok(then) = DateTime::parse_from_rfc3339(created_at) else {
        return created_at.to_string();
    };
    let secs = (Utc::now() - then.with_timezone(&Utc)).num_seconds().max(0);
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// The excerpt source: `-` reads stdin, a path reads the file, absent is None.
fn read_excerpt(source: &Option<String>) -> Result<Option<String>, String> {
    match source.as_deref() {
        None => Ok(None),
        Some("-") => {
            use std::io::Read;
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .map_err(|e| format!("excerpt stdin read failed: {e}"))?;
            Ok(Some(s))
        }
        Some(path) => Ok(Some(
            std::fs::read_to_string(path).map_err(|e| format!("excerpt file read failed: {e}"))?,
        )),
    }
}

/// Telemetry after commit, best-effort: both journals, pointer payload only
/// (the store holds the body).
fn emit_finding_event(event_type: &str, data: Value) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project = crate::paths::events_path(&cwd);
    let global = crate::loopcheck::default_global_events_path();
    crate::loopcheck::emit_to_both(&project, &global, event_type, data);
}

/// Set the bounded `state_needs_refresh` marker in the row extras. Runs the
/// shared optimistic cycle: the marker now fails loud instead of a
/// dropped Result on a lost race.
fn set_refresh_marker(
    graph: &std::path::Path,
    node_id: &str,
) -> Result<(), graph_store::StoreError> {
    graph_store::mutate_rows(
        graph,
        std::time::Duration::from_secs(5),
        None,
        None,
        |rows| {
            let Some(row) = rows
                .iter_mut()
                .find(|r| graph_store::entry_id(r) == Some(node_id))
            else {
                return Ok(false);
            };
            if let Some(obj) = row.as_object_mut() {
                obj.insert("state_needs_refresh".into(), json!(true));
            }
            Ok(true)
        },
    )
    .map(|_| ())
}

/// The human path: resolve readers, refuse before write when nobody is bound
/// and not quiet, replace the state, print the receipt with the pointer.
fn write_human(
    parsed: &NoteArgs,
    graph: &std::path::Path,
    entry: &Value,
    body: Option<String>,
) -> i32 {
    let Some(body) = body else {
        eprintln!(
            "fno-agents backlog-note: a note needs a body (positional, --body-file, or --stdin)"
        );
        return 2;
    };
    let node_id = entry
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // --clear stays the one state route: it empties current_state, it does
    // not append to the thread. The cross-session guard survives here: a
    // clear over a revision this session cannot prove it wrote refuses
    // until --if-revision names that revision deliberate.
    if parsed.clear {
        if let Some(p) = node_state::read_state(entry) {
            let mine = p.source_session_id.as_deref() == parsed.self_session.as_deref();
            if !mine && parsed.if_revision != Some(p.revision) {
                eprintln!(
                    "Error: note --clear refused: current state on {node_id} is revision {}, \
written by session {}. Nothing was cleared. Pass --if-revision {} to clear it deliberately.",
                    p.revision,
                    p.source_session_id
                        .as_deref()
                        .unwrap_or("an unknown session"),
                    p.revision,
                );
                return 3;
            }
        }
        let rev = node_state::current_revision(graph, &node_id).unwrap_or(0);
        let submitted = parsed.if_revision.unwrap_or(rev);
        if let Err(e) = node_state::clear_state(graph, &node_id, Some(submitted)) {
            eprintln!("fno-agents backlog-note: {e}");
            return map_state_err(&e);
        }
        if parsed.json_out {
            crate::backlog::receipt::emit_line(
                &json!({"status": "ok", "routed": "clear", "node_id": node_id, "revision": rev})
                    .to_string(),
            );
        } else {
            crate::backlog::receipt::emit_line(&format!(
                "cleared {node_id}: current state cleared"
            ));
        }
        return 0;
    }
    if parsed.replace {
        eprintln!(
            "fno-agents backlog-note: --replace is retired: a note appends to the \
thread and cannot clobber anything. Read the feed: fno backlog note comment {node_id} --list"
        );
        return 3;
    }
    if parsed.if_revision.is_some() {
        eprintln!(
            "fno-agents backlog-note: --if-revision guards --clear only: a note \
appends to the thread and cannot conflict"
        );
        return 2;
    }
    let body = node_state::normalize_prose(&body);
    if body.is_empty() {
        eprintln!("Error: note text is empty");
        return 1;
    }
    let kind = parsed
        .kind
        .clone()
        .unwrap_or_else(|| "progress".to_string());
    if !NOTE_KINDS.contains(&kind.as_str()) {
        eprintln!(
            "fno-agents backlog-note: --kind must be one of {} (default progress)",
            NOTE_KINDS.join(", ")
        );
        return 2;
    }
    let ident = comment_identity(parsed.self_session.as_deref());
    let reads: Option<Value> = parsed
        .reads
        .as_deref()
        .and_then(|r| serde_json::from_str(r).ok());
    let input = super::api::CommentCreateInput {
        body: body.clone(),
        kind: Some(kind.clone()),
        author: Some("agent".to_string()),
        session_id: ident.session_id,
        harness: ident.harness,
        agent_name: ident.agent_name,
        model: ident.model,
        working_node: ident.working_node,
        reads,
        ..Default::default()
    };
    let store = super::api::Store::new(graph);
    match super::api::comment_create(&store, &node_id, input) {
        // A refusal payload (success false) is not a landed note: the
        // receipt must never print ok over one.
        Ok(payload) if payload.success => {
            let line = format!(
                "noted {node_id}: {kind} appended to the thread; \
read the feed: fno backlog note comment {node_id} --list"
            );
            emit_human(
                parsed.json_out,
                &json!({
                    "status": "ok", "routed": "thread", "node_id": node_id, "id": node_id,
                    "kind": kind, "text": body, "line": line,
                }),
            );
            0
        }
        Ok(_) => {
            eprintln!(
                "fno-agents backlog-note: the append refused and nothing was written to {node_id}"
            );
            1
        }
        Err(e) => {
            eprintln!("fno-agents backlog-note: {}", e.0);
            1
        }
    }
}

/// The writer identity a thread row stamps: what this process can
/// prove, plus the model and working node the fleet already knows. An
/// absent field stays absent - an honest unknown beats a wrong label.
struct CommentIdentity {
    session_id: Option<String>,
    harness: Option<String>,
    agent_name: Option<String>,
    model: Option<String>,
    working_node: Option<String>,
}

fn comment_identity(self_session: Option<&str>) -> CommentIdentity {
    let ident = crate::spawn_context::resolve_self_identity(
        &|k| std::env::var(k).ok(),
        None,
        None,
        &crate::paths::AgentsHome::from_env(),
    );
    let session_id = self_session
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| ident.session_id.clone())
        .filter(|s| !s.is_empty());
    let harness = ident.harness.clone().filter(|h| !h.is_empty());
    // The claim this session holds names the node it is working and, in the
    // spawn-handover holder form, the worker name. The observed model is
    // not resolved here: the write's own mutation reads it off the row it
    // already loads, so identity resolution costs no store read.
    let mut working_node = None;
    let mut holder_name = None;
    if let Some(sid) = session_id.as_deref() {
        if let Ok(claims) = crate::backlog::nodes::node_claims_by_id() {
            for (node_id, claim) in &claims {
                if claim.harness_session.as_deref() != Some(sid) {
                    continue;
                }
                working_node = Some(node_id.clone());
                if let Some(name) = claim
                    .locked_by
                    .as_deref()
                    .and_then(|h| h.strip_prefix("spawn-handover:"))
                {
                    holder_name = Some(name.to_string());
                }
                break;
            }
        }
    }
    let agent_name = std::env::var("FNO_AGENT_NAME")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .or(holder_name);
    CommentIdentity {
        session_id,
        harness,
        agent_name,
        model: None,
        working_node,
    }
}

/// Print one human receipt: the object under --json, else its `line`.
fn emit_human(json_out: bool, receipt: &Value) {
    let line = if json_out {
        receipt.to_string()
    } else {
        receipt["line"].as_str().unwrap_or("").to_string()
    };
    crate::backlog::receipt::emit_line(&line);
}

/// The --import-record route: read, validate, refuse-or-write, emit the
/// receipt. Exit 0 written, 1 refused or failed (nothing written), 2 usage
/// (handled at the parse layer).
fn run_import_record(parsed: &NoteArgs, graph: &Path, source: &str) -> i32 {
    let record = match read_record_source(source) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fno-agents backlog-note: {e}");
            return 1;
        }
    };
    match import_record(graph, &record) {
        Ok(receipt) => {
            emit_human(parsed.json_out, &receipt);
            0
        }
        Err(e) => {
            eprintln!("fno-agents backlog-note: {e}");
            1
        }
    }
}

/// The record source: `-` reads stdin, a path reads the file.
fn read_record_source(source: &str) -> Result<Value, String> {
    let text = if source == "-" {
        use std::io::Read;
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| format!("record stdin read failed: {e}"))?;
        s
    } else {
        std::fs::read_to_string(source).map_err(|e| format!("record file read failed: {e}"))?
    };
    serde_json::from_str(&text).map_err(|e| format!("record is not valid JSON: {e}"))
}

/// Validate and import one captured record under its original id and slug.
/// The receipt carries the restored identity and the store's mutation
/// counter; a refusal is the self-teaching message and the guarantee that
/// nothing was written.
fn import_record(graph: &Path, record: &Value) -> Result<Value, String> {
    let db = crate::backlog::database_path(graph);
    if !db.exists() {
        return Err(format!(
            "no store at {}; nothing was written. An import repairs an \
             existing store, it never creates one",
            db.display()
        ));
    }
    let node = Node::from_json(record).map_err(|e| {
        format!(
            "record does not validate: {e}. Nothing was written. A captured \
             record is the `fno backlog get` output"
        )
    })?;
    let id = node.id.clone();
    let slug = node.slug.clone();
    let mut refusal: Option<String> = None;
    let ok = crate::backlog::mutate_single_row(graph, "node_import", |rows| {
        if rows
            .iter()
            .any(|r| graph_store::entry_id(r) == Some(id.as_str()))
        {
            refusal = Some(format!(
                "refusing: node {id} is already live in the store; nothing was \
                 written. Read it: fno backlog get {id}. An import restores a \
                 lost record, it never overwrites"
            ));
            return Ok(false);
        }
        if rows.iter().any(|r| {
            graph_store::entry_id(r) != Some(id.as_str())
                && r.get("slug").and_then(Value::as_str) == Some(slug.as_str())
        }) {
            refusal = Some(format!(
                "refusing: slug {slug} is already held by another node; nothing \
                 was written. Slugs are unique in the store"
            ));
            return Ok(false);
        }
        rows.push(record.clone());
        Ok(true)
    })?;
    if !ok {
        return Err(refusal.unwrap_or_else(|| "import refused".to_string()));
    }
    let version = crate::backlog::api_version(graph)?;
    Ok(json!({
        "status": "ok",
        "routed": "import",
        "id": id,
        "slug": slug,
        "version": version,
        "line": format!("imported {id} (slug {slug})"),
    }))
}

/// Map a state-write error to the verb's exit code.
fn map_state_err(e: &StateError) -> i32 {
    match e {
        StateError::NoNode(_) => 1,
        StateError::Conflict { .. } => 3,
        StateError::EmptyBody => 1,
        StateError::History(_) => 1,
        StateError::Store(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn json_flag_accepts_both_spellings() {
        let long = parse_args(&args(&["x-1", "--json"])).unwrap();
        let short = parse_args(&args(&["x-1", "-J"])).unwrap();
        assert!(long.json_out);
        assert!(short.json_out);
    }

    #[test]
    fn an_unknown_flag_still_refuses() {
        assert!(parse_args(&args(&["x-1", "-j"])).is_err());
        assert!(parse_args(&args(&["x-1", "--JSON"])).is_err());
    }

    /// a note appends a thread row and leaves current_state alone;
    /// the node's journal rides into the thread on the first append; a
    /// second note adds a row without migrating again; --replace refuses.
    #[test]
    fn a_note_appends_a_thread_row_and_leaves_the_state() {
        let (_lock, _root) = claims_root_pin();
        let (_dir, graph) = comment_graph();
        let graph_s = graph.to_string_lossy().into_owned();
        // A prior state and a journal record: exactly what a pre-flip node
        // carries, so the append proves both untouched-or-migrated.
        node_state::replace_state(
            graph.as_path(),
            &node_state::StateWriteInput {
                node_id: "x-t1".into(),
                body: "prior state".into(),
                if_revision: Some(0),
                source_session_id: Some("sess-old".into()),
                source_harness: None,
                reads: None,
            },
        )
        .unwrap();
        note_history::append(
            graph.as_path(),
            "x-t1",
            note_history::REASON_STATE_REPLACED,
            Some(1),
            None,
            &json!({"revision": 1, "body": "prior state"}),
            Some("sess-old"),
            None,
        )
        .unwrap();
        let rc = run_note(&argv(&[
            "--graph",
            &graph_s,
            "x-t1",
            "hello thread",
            "--kind",
            "finding",
            "--self-session",
            "sess-me",
        ]));
        assert_eq!(rc, 0, "the note appends");
        let store = super::super::api::Store::new(graph.as_path());
        let thread =
            super::super::api::comments(&store, "x-t1", &super::super::api::Page::default())
                .unwrap();
        assert_eq!(
            thread.nodes.len(),
            2,
            "the migrated journal row plus the new finding"
        );
        assert_eq!(
            thread.nodes[0]
                .extras
                .get("migrated")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            thread.nodes[0].body.as_deref(),
            Some("prior state"),
            "the journal record verbatim"
        );
        assert_eq!(thread.nodes[1].kind.as_deref(), Some("finding"));
        let state = node_state::read_state(
            &graph_store::read_rows(graph.as_path())
                .unwrap()
                .into_iter()
                .find(|r| graph_store::entry_id(r) == Some("x-t1"))
                .unwrap(),
        )
        .expect("state still present");
        assert_eq!(state.body, "prior state", "the state is untouched");
        assert_eq!(state.revision, 1);
        // --replace refuses: appends cannot clobber.
        let rc = run_note(&argv(&[
            "--graph",
            &graph_s,
            "x-t1",
            "clobber",
            "--replace",
        ]));
        assert_eq!(rc, 3, "--replace refuses");
    }

    // -- import route -----------------------------------------------------

    fn fixture(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let graph = dir.path().join(name);
        (dir, graph)
    }

    /// The captured-record shape: what `fno backlog get` emits, sessions and
    /// provenance included.
    fn captured_record(id: &str, slug: &str) -> Value {
        serde_json::json!({
            "id": id, "slug": slug, "title": "Lost", "type": "feature",
            "status": "in_progress", "priority": "p1", "project": "fno",
            "domain": "code", "difficulty": "medium",
            "details": "restored from a captured record",
            "created_at": "2026-09-16T04:38:39.696103+00:00",
            "source_kind": "operator_request",
            "sessions": [
                {"phase": "do", "harness": "claude", "session_id": "s-1"}
            ],
        })
    }

    fn seed_one_node(graph: &std::path::Path) {
        let rows = serde_json::json!({"entries": [
            {"id": "ab-one", "slug": "one", "title": "One", "type": "feature",
             "status": "idea", "priority": "p2", "domain": "code",
             "created_at": "2026-09-11T00:00:00+00:00"}
        ]});
        graph_store::seed_rows(graph, rows["entries"].as_array().unwrap()).unwrap();
    }

    #[test]
    fn an_import_lands_the_record_under_its_original_id_and_slug() {
        let (_dir, graph) = fixture("graph.json");
        seed_one_node(&graph);
        let receipt = import_record(&graph, &captured_record("ab-lost", "lost")).unwrap();
        assert_eq!(receipt["id"], "ab-lost");
        assert_eq!(receipt["slug"], "lost");
        let rows = graph_store::read_rows(&graph).unwrap();
        let restored = rows
            .iter()
            .find(|r| graph_store::entry_id(r) == Some("ab-lost"))
            .expect("the imported node is live");
        assert_eq!(restored["slug"], "lost");
        assert_eq!(restored["title"], "Lost");
        assert_eq!(
            restored["sessions"][0]["session_id"], "s-1",
            "the aggregate's child rows ride the import"
        );
    }

    #[test]
    fn an_import_refuses_when_the_id_is_already_live() {
        let (_dir, graph) = fixture("graph.json");
        seed_one_node(&graph);
        let error = import_record(&graph, &captured_record("ab-one", "renamed")).unwrap_err();
        assert!(error.contains("ab-one is already live"), "{error}");
        assert!(error.contains("fno backlog get ab-one"), "{error}");
        let rows = graph_store::read_rows(&graph).unwrap();
        assert_eq!(rows.len(), 1, "nothing was written");
    }

    #[test]
    fn an_import_refuses_when_the_slug_is_held_elsewhere() {
        let (_dir, graph) = fixture("graph.json");
        seed_one_node(&graph);
        let error = import_record(&graph, &captured_record("ab-other", "one")).unwrap_err();
        assert!(error.contains("slug one is already held"), "{error}");
        let rows = graph_store::read_rows(&graph).unwrap();
        assert_eq!(rows.len(), 1, "nothing was written");
    }

    #[test]
    fn an_import_refuses_a_record_that_fails_the_schema() {
        let (_dir, graph) = fixture("graph.json");
        seed_one_node(&graph);
        let mut record = captured_record("ab-bad", "bad");
        record.as_object_mut().unwrap().remove("title");
        let error = import_record(&graph, &record).unwrap_err();
        assert!(error.contains("record does not validate"), "{error}");
        let rows = graph_store::read_rows(&graph).unwrap();
        assert_eq!(rows.len(), 1, "nothing was written");
    }

    #[test]
    fn an_import_refuses_a_missing_store() {
        let (_dir, graph) = fixture("graph.json");
        let error = import_record(&graph, &captured_record("ab-lost", "lost")).unwrap_err();
        assert!(error.contains("no store at"), "{error}");
        assert!(
            !crate::backlog::database_path(&graph).exists(),
            "created nothing"
        );
    }

    #[test]
    fn usage_refuses_an_unknown_flag() {
        let args: Vec<String> = ["record.json", "--dry-run"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_args(&args).is_err());
    }

    fn comment_graph() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        let store = super::super::api::Store::new(&graph);
        super::super::api::node_create(
            &store,
            super::super::api::NodeCreateInput {
                id: "x-t1".into(),
                title: "thread fixture".into(),
                ..Default::default()
            },
        )
        .unwrap();
        (dir, graph)
    }

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// The comment test reads the claims state through `notice_holder`'s
    /// holder probe; the hermetic guard demands a temp root, and the env is
    /// process-global, so the tests serialize on one mutex.
    fn claims_root_pin() -> (std::sync::MutexGuard<'static, ()>, tempfile::TempDir) {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let guard = LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = tempfile::tempdir().unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", root.path());
        (guard, root)
    }

    #[test]
    fn comment_thread_cli_contract() {
        let (_lock, _root) = claims_root_pin();
        let (_dir, graph) = comment_graph();
        let graph = graph.to_string_lossy().into_owned();
        let rc = run_comment(&argv(&[
            "--graph",
            &graph,
            "x-t1",
            "rename the flag",
            "--author",
            "user",
        ]));
        assert_eq!(rc, 0, "a user comment posts");
        let store = super::super::api::Store::new(graph.as_ref());
        let read = || {
            super::super::api::comments(&store, "x-t1", &super::super::api::Page::default())
                .unwrap()
        };
        let thread = read();
        assert_eq!(thread.nodes.len(), 1);
        assert_eq!(
            thread.nodes[0].extras.get("author").and_then(Value::as_str),
            Some("user")
        );
        assert_eq!(
            thread.nodes[0].extras.get("state").and_then(Value::as_str),
            Some("open")
        );
        let cid = thread.nodes[0]
            .extras
            .get("comment_id")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        let rc = run_comment(&argv(&[
            "--graph", &graph, "x-t1", "--reply", &cid, "--state", "done", "landed",
        ]));
        assert_eq!(rc, 1, "done without a ref refuses");
        let rc = run_comment(&argv(&[
            "--graph",
            &graph,
            "x-t1",
            "--reply",
            &cid,
            "--state",
            "accepted",
            "--ref",
            "node x-9 filed",
            "on it",
        ]));
        assert_eq!(rc, 0, "an accepted reply with a ref lands");
        let thread = read();
        assert_eq!(thread.nodes.len(), 2);
        assert_eq!(
            thread.nodes[0].extras.get("state").and_then(Value::as_str),
            Some("accepted")
        );
    }
}
