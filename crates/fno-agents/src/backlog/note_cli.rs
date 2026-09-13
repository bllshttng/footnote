//! `fno-agents backlog-note` (x-920a wave 2): the native note action the
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
            "--json" => out.json_out = true,
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
/// nobody-bound refusal (nothing written), 0 written.
pub fn run_note(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno-agents backlog-note: {e}");
            return 2;
        }
    };
    let graph = parsed.graph.clone().unwrap_or_else(default_graph_path);
    let body = match read_body(&parsed) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("fno-agents backlog-note: {e}");
            return 1;
        }
    };
    let entries = match graph_store::read_defaulted(&graph, false) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("fno-agents backlog-note: graph read failed: {e}");
            return 1;
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
        set_refresh_marker(graph, node_id);
    }
    if parsed.json_out {
        println!(
            "{}",
            json!({
                "status": "ok",
                "routed": "history",
                "node_id": node_id,
                "revision": rev,
            })
        );
    } else {
        println!("recorded {node_id}: history");
    }
    0
}

/// Set the bounded `state_needs_refresh` marker in the row extras.
fn set_refresh_marker(graph: &std::path::Path, node_id: &str) {
    let Ok(rows) = graph_store::read_defaulted(graph, false) else {
        return;
    };
    let mut working = rows;
    for row in working.iter_mut() {
        if graph_store::entry_id(row) == Some(node_id) {
            if let Some(obj) = row.as_object_mut() {
                obj.insert("state_needs_refresh".into(), json!(true));
            }
            break;
        }
    }
    let _ = graph_store::locked_mutate(
        graph,
        graph_store::MutateInput {
            entries: working,
            canonical_path: None,
            base_version: None,
            plan_rungs: None,
        },
        std::time::Duration::from_secs(5),
    );
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
        if parsed.json_out {
            println!(
                "{}",
                json!({
                    "status": "ok",
                    "routed": "history",
                    "node_id": node_id,
                    "revision": rev,
                })
            );
        } else {
            println!("recorded {node_id}: history (node is done or superseded)");
        }
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
            println!(
                "{}",
                json!({"status": "ok", "routed": "clear", "node_id": node_id, "revision": rev})
            );
        } else {
            println!("cleared {node_id}: current state cleared");
        }
        return 0;
    }
    // The revision the CLI submits: explicit --if-revision, else the fetched
    // current revision (the optimistic-concurrency guard is always on).
    let rev = node_state::current_revision(graph, &node_id).unwrap_or(0);
    let submitted = parsed.if_revision.unwrap_or(rev);
    let input = StateWriteInput {
        node_id: node_id.clone(),
        body,
        if_revision: Some(submitted),
        source_session_id: parsed.self_session.clone(),
        source_harness: None,
    };
    let receipt = match node_state::replace_state(graph, &input) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fno-agents backlog-note: {e}");
            return map_state_err(&e);
        }
    };
    if parsed.json_out {
        println!(
            "{}",
            json!({
                "status": "ok",
                "routed": "state",
                "node_id": receipt.node_id,
                "revision": receipt.revision,
                "journaled": receipt.journaled,
                "total_prose": receipt.total_prose,
            })
        );
    } else {
        println!("noted {}: revision {}", receipt.node_id, receipt.revision);
    }
    0
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
