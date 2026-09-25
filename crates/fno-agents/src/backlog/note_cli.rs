//! `fno-agents backlog-note` (wave 2): the native note action the
//! Python `fno backlog note` bridge calls. The Rust side owns the bounded
//! state policy, revision-checked replacement, history routing (machine,
//! wave, terminal), and the combined-prose budget; the bridge keeps the
//! shipped recipient walk (`note_notify`, its test contract), evidence
//! checks, identity, archived refusal, and the mail transport.
use crate::backlog::node_state::{self, StateError, StateWriteInput};
use crate::backlog::note_history;
use crate::graph_store::{self};
use serde_json::{json, Value};
use std::path::PathBuf;

use crate::graph_get::default_graph_path;

/// One parsed invocation of the note action.
struct NoteArgs {
    node: String,
    body: Option<String>,
    body_file: Option<String>,
    stdin: bool,
    read: bool,
    clear: bool,
    quiet: bool,
    json_out: bool,
    if_revision: Option<u64>,
    machine: Option<String>,
    wave: bool,
    refresh_marker: bool,
    graph: Option<PathBuf>,
    self_session: Option<String>,
    reads: Option<String>,
    blocking: bool,
    resolve: Option<String>,
    block_cmd: Option<String>,
    block_excerpt_file: Option<String>,
}

fn parse_args(args: &[String]) -> Result<NoteArgs, String> {
    let mut out = NoteArgs {
        node: String::new(),
        body: None,
        body_file: None,
        stdin: false,
        read: false,
        clear: false,
        quiet: false,
        json_out: false,
        if_revision: None,
        machine: None,
        wave: false,
        refresh_marker: false,
        graph: None,
        self_session: None,
        reads: None,
        blocking: false,
        resolve: None,
        block_cmd: None,
        block_excerpt_file: None,
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
/// JSON receipt on stdout. Exit 2 usage, 1 missing graph/node, 3 the
/// nobody-bound refusal (nothing written), 5 graph read failed, 0 written.
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
fn notice_holder(node_id: &str, pointer: &str) -> Option<String> {
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
    // Terminal nodes: the note goes to history only, never hot state.
    let status = entry.get("status").and_then(Value::as_str).unwrap_or("");
    if matches!(status, "done" | "superseded") {
        let node_id = entry.get("id").and_then(Value::as_str).unwrap_or("");
        let rev = node_state::current_revision(graph, node_id).unwrap_or(0);
        let original = json!({
            "body": node_state::normalize_prose(&body),
            "ts": graph_store::now_isoformat(),
            "source_session_id": parsed.self_session,
        });
        if let Err(e) = note_history::append(
            graph,
            node_id,
            note_history::REASON_TERMINAL_EVACUATED,
            Some(rev),
            None,
            &original,
            parsed.self_session.as_deref(),
            None,
        ) {
            eprintln!("fno-agents backlog-note: history write failed: {e}");
            return 1;
        }
        let out = json!({
            "status": "ok", "routed": "history", "node_id": node_id, "id": node_id,
            "text": node_state::normalize_prose(&body), "revision": rev, "replaced": Value::Null,
            "line": format!("recorded {node_id}: history (node is done or superseded)"),
        });
        emit_human(parsed.json_out, &out);
        return 0;
    }
    let node_id = entry
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let body = node_state::normalize_prose(&body);
    if body.is_empty() && !parsed.clear {
        eprintln!("Error: note text is empty");
        return 1;
    }
    if parsed.clear {
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
    // The revision the CLI submits: explicit --if-revision, else the fetched
    // current revision (the optimistic-concurrency guard is always on).
    let rev = node_state::current_revision(graph, &node_id).unwrap_or(0);
    let submitted = parsed.if_revision.unwrap_or(rev);
    let reads: Option<Value> = parsed
        .reads
        .as_deref()
        .and_then(|r| serde_json::from_str(r).ok());
    let text = body.clone();
    let chars = body.chars().count();
    let input = StateWriteInput {
        node_id: node_id.clone(),
        body,
        if_revision: Some(submitted),
        source_session_id: parsed.self_session.clone(),
        source_harness: None,
        reads,
    };
    let receipt = match node_state::replace_state(graph, &input) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fno-agents backlog-note: {e}");
            return map_state_err(&e);
        }
    };
    let (replaced, replaced_line) = replaced_parts(&receipt.node_id, receipt.replaced.as_ref());
    let mut line = format!(
        "noted {}: revision {}, {chars} chars\n{replaced_line}",
        receipt.node_id, receipt.revision
    );
    // The encounters snapshot rides the receipt from inside the publication
    // lock, so the hint can never fire on an encounter the write could not
    // see.
    let hint = encounter_hint(
        &receipt.node_id,
        &receipt.encounters,
        parsed.self_session.as_deref(),
        receipt.replaced.as_ref(),
    );
    if let Some(h) = hint {
        line.push('\n');
        line.push_str(&h);
    }
    let out = json!({
        "status": "ok", "routed": "state", "node_id": receipt.node_id, "id": receipt.node_id,
        "text": text, "revision": receipt.revision, "journaled": receipt.journaled,
        "total_prose": receipt.total_prose, "replaced": replaced,
        "line": line,
    });
    emit_human(parsed.json_out, &out);
    0
}

/// The first line of a body, cut at 80 characters; "..." marks what is left out.
fn head(body: &str) -> String {
    let mut out: String = body.lines().next().unwrap_or("").chars().take(80).collect();
    if out.len() < body.len() {
        out.push_str("...");
    }
    out
}

/// What a human note replaced, as JSON and as one receipt line.
fn replaced_parts(node_id: &str, prior: Option<&node_state::CurrentStateView>) -> (Value, String) {
    let Some(p) = prior else {
        return (
            Value::Null,
            format!("replaced nothing: {node_id} had no current state"),
        );
    };
    let chars = p.body.chars().count();
    let excerpt = head(&p.body);
    let author = p.source_session_id.as_deref().unwrap_or("unknown");
    let when = p.updated_at.as_deref().unwrap_or("an unknown time");
    let json = json!({
        "revision": p.revision, "chars": chars, "source_session_id": &p.source_session_id,
        "updated_at": &p.updated_at, "head": &excerpt,
    });
    let line = format!(
        "replaced revision {} ({chars} chars, written by session {author} at {when}): \"{excerpt}\". Read it back: fno backlog notes history {node_id}",
        p.revision
    );
    (json, line)
}

/// The repeat-note nudge: when the session writing this note also wrote the
/// state it replaces, and that session has no encounter on the node yet, name
/// the verb that feeds `fno backlog demand`. Teaching at the point of use,
/// never a gate.
fn encounter_hint(
    node_id: &str,
    encounters: &Value,
    self_session: Option<&str>,
    prior: Option<&node_state::CurrentStateView>,
) -> Option<String> {
    let s = self_session.filter(|s| !s.is_empty())?;
    let p = prior?;
    if p.source_session_id.as_deref() != Some(s) {
        return None;
    }
    let already = encounters
        .as_array()
        .map(|items| {
            items.iter().any(|e| {
                e.get("session_id").and_then(Value::as_str) == Some(s)
                    || e.get("voter_key").and_then(Value::as_str) == Some(s)
            })
        })
        .unwrap_or(false);
    if already {
        return None;
    }
    Some(format!(
        "this session noted {node_id} before and has no encounter on it. If it cost you, record that: fno backlog encounter {node_id} --evidence \"<what it cost>\""
    ))
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

    fn view(rev: u64, session: Option<&str>) -> node_state::CurrentStateView {
        node_state::CurrentStateView {
            revision: rev,
            body: "prior state".into(),
            updated_at: Some("2026-09-22T00:00:00+00:00".into()),
            source_session_id: session.map(|s| s.to_string()),
            source_harness: None,
        }
    }

    #[test]
    fn same_author_and_no_encounter_hints_encounter() {
        let hint = encounter_hint(
            "t-1",
            &json!(null),
            Some("sess-a"),
            Some(&view(3, Some("sess-a"))),
        );
        let hint = hint.expect("same author, no encounter: hint fires");
        assert!(hint.contains("fno backlog encounter t-1 --evidence"));
    }

    #[test]
    fn a_matching_encounter_silences_the_hint() {
        let by_session = json!([
            {"ts": "2026-09-22T00:00:00+00:00", "evidence": "x", "session_id": "sess-a"}
        ]);
        assert!(encounter_hint(
            "t-1",
            &by_session,
            Some("sess-a"),
            Some(&view(3, Some("sess-a")))
        )
        .is_none());
        let by_voter = json!([
            {"ts": "2026-09-22T00:00:00+00:00", "evidence": "x", "voter_key": "sess-a"}
        ]);
        assert!(encounter_hint(
            "t-1",
            &by_voter,
            Some("sess-a"),
            Some(&view(3, Some("sess-a")))
        )
        .is_none());
    }

    #[test]
    fn a_different_prior_author_gets_no_hint() {
        assert!(encounter_hint(
            "t-1",
            &json!(null),
            Some("sess-b"),
            Some(&view(3, Some("sess-a")))
        )
        .is_none());
    }

    #[test]
    fn no_self_session_gets_no_hint() {
        assert!(
            encounter_hint("t-1", &json!(null), None, Some(&view(3, Some("sess-a")))).is_none()
        );
    }
}
