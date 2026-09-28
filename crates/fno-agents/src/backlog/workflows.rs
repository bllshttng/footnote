//! The lifecycle close and its inverse: `backlog done` (the canonical
//! close-flag surface) and `backlog reopen`, the door-side port of
//! cmd_done/cmd_reopen. The rich completion flags keep their Python surface
//! (done_command rides the forward); the close-flag surface answers here,
//! with the same gates (gh merge evidence, promise), the same receipts, and
//! the same cascade semantics.

use serde_json::{json, Value};
use std::cell::RefCell;
use std::path::Path;

use super::cli::forward_to_python;
use super::merge_evidence::{
    node_pr_refs, query_pr_state, render_merge_evidence_failure, repo_slug_from_url,
    resolve_merge_evidence, Outcome,
};
use super::node_ref::has_node_id_prefix;
use super::promise::resolve_promise_evidence;
use super::settings;
use crate::backlog::mutate_single_row;
use crate::graph_store;

// ---------------------------------------------------------------------------
// argv parsing
// ---------------------------------------------------------------------------

/// The parsed `done` argv. Unknown flags set `unknown`, which keeps Python
/// (the owner of every usage error and help text) on the path.
struct DoneArgs {
    id: Option<String>,
    force: bool,
    reason: Option<String>,
    skip_stamp: bool,
    pr: Option<i64>,
    pr_url: Option<String>,
    repo: Option<String>,
    link: Option<String>,
    note: Option<String>,
    backfill: bool,
    force_overwrite: bool,
    unknown: bool,
}

impl DoneArgs {
    fn close_flags(&self) -> bool {
        self.force || self.reason.is_some() || self.skip_stamp
    }

    fn rich_flags(&self) -> bool {
        self.backfill
            || self.force_overwrite
            || self.pr.is_some()
            || self.pr_url.is_some()
            || self.repo.is_some()
            || self.link.is_some()
            || self.note.is_some()
    }
}

fn take_value(args: &[String], i: &mut usize) -> Option<String> {
    *i += 1;
    args.get(*i).cloned()
}

fn parse_done_args(tail: &[String]) -> DoneArgs {
    let mut a = DoneArgs {
        id: None,
        force: false,
        reason: None,
        skip_stamp: false,
        pr: None,
        pr_url: None,
        repo: None,
        link: None,
        note: None,
        backfill: false,
        force_overwrite: false,
        unknown: false,
    };
    let mut i = 0usize;
    while i < tail.len() {
        let tok = tail[i].as_str();
        match tok {
            "--force" | "-F" => a.force = true,
            "--skip-stamp" => a.skip_stamp = true,
            "--backfill" => a.backfill = true,
            "--force-overwrite" => a.force_overwrite = true,
            "--reason" | "-R" => a.reason = take_value(tail, &mut i),
            "--pr-number" | "--pr" | "-p" => {
                a.pr = take_value(tail, &mut i).and_then(|v| v.parse().ok())
            }
            "--pr-url" => a.pr_url = take_value(tail, &mut i),
            "--repo" => a.repo = take_value(tail, &mut i),
            "--link" | "--url" | "-l" => a.link = take_value(tail, &mut i),
            "--note" | "-m" => a.note = take_value(tail, &mut i),
            t if t.starts_with("--reason=") => a.reason = t.split('=').nth(1).map(String::from),
            t if t.starts_with("--pr=") => a.pr = t.split('=').nth(1).and_then(|v| v.parse().ok()),
            t if t.starts_with("--note=") => a.note = t.split('=').nth(1).map(String::from),
            t if t.starts_with("--link=") => a.link = t.split('=').nth(1).map(String::from),
            t if t.starts_with("--pr-url=") => a.pr_url = t.split('=').nth(1).map(String::from),
            t if t.starts_with("--repo=") => a.repo = t.split('=').nth(1).map(String::from),
            t if t.starts_with('-') && t != "-" => a.unknown = true,
            _ => {
                if a.id.is_none() {
                    a.id = Some(tok.to_string());
                } else {
                    a.unknown = true;
                }
            }
        }
        i += 1;
    }
    a
}

fn parse_reopen_args(tail: &[String]) -> (Option<String>, Option<String>, bool, bool, bool) {
    let mut id = None;
    let mut reason = None;
    let mut force = false;
    let mut help = false;
    let mut unknown = false;
    let mut i = 0usize;
    while i < tail.len() {
        match tail[i].as_str() {
            "--reason" | "-R" => reason = take_value(tail, &mut i),
            "--force" | "-F" => force = true,
            "--help" | "-h" => help = true,
            t if t.starts_with("--reason=") => reason = t.split('=').nth(1).map(String::from),
            t if t.starts_with('-') && t != "-" => unknown = true,
            _ => {
                if id.is_none() {
                    id = Some(tail[i].clone());
                } else {
                    unknown = true;
                }
            }
        }
        i += 1;
    }
    (id, reason, force, help, unknown)
}

// ---------------------------------------------------------------------------
// shared graph helpers (the strand/reopen twins)
// ---------------------------------------------------------------------------

/// LIVE = not terminal: no completion, no deferral, and a supersession that
/// is either absent or unverified (the _is_live twin).
fn is_live(entry: &Value) -> bool {
    if truthy_field(entry, "completed_at") || truthy_field(entry, "deferred_at") {
        return false;
    }
    if entry.get("superseded_by").and_then(Value::as_str).is_none() {
        return true;
    }
    let supersession = entry.get("supersession");
    supersession
        .map(|s| s.is_object() && !truthy_field(s, "verified_at"))
        .unwrap_or(false)
}

fn truthy_field(row: &Value, key: &str) -> bool {
    row.get(key)
        .map(|v| !v.is_null() && v.as_bool() != Some(false))
        .unwrap_or(false)
}

fn text_at<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    row.get(key).and_then(Value::as_str)
}

/// Ids of the owner's live membership children; contained children never
/// count (the _live_child_ids twin).
fn live_child_ids(entries: &[Value], owner_id: &str) -> Vec<String> {
    let mut live = Vec::new();
    for e in entries {
        if !e.is_object() {
            continue;
        }
        if text_at(e, "contained_in") == Some(owner_id) || text_at(e, "parent") != Some(owner_id) {
            continue;
        }
        if is_live(e) {
            if let Some(id) = text_at(e, "id") {
                if !id.is_empty() {
                    live.push(id.to_string());
                }
            }
        }
    }
    live
}

/// Re-parent each live membership child of `dead_id` onto the nearest live
/// ancestor (the _reparent_live_children twin). Returns the moved pairs.
fn reparent_live_children(entries: &mut [Value], dead_id: &str) -> Vec<(String, Option<String>)> {
    let kids = live_child_ids(entries, dead_id);
    if kids.is_empty() {
        return Vec::new();
    }
    let mut target: Option<String> = None;
    let mut seen: Vec<String> = Vec::new();
    let mut cur = entries
        .iter()
        .find(|e| text_at(e, "id") == Some(dead_id))
        .and_then(|e| text_at(e, "parent").map(String::from));
    let mut steps = 0;
    while let Some(c) = cur.clone() {
        if c.is_empty() || c == dead_id || seen.contains(&c) || steps >= 64 {
            break;
        }
        seen.push(c.clone());
        let anc = entries
            .iter()
            .find(|e| text_at(e, "id") == Some(c.as_str()));
        match anc {
            None => break,
            Some(a) => {
                if is_live(a) {
                    target = Some(c);
                    break;
                }
                cur = text_at(a, "parent").map(String::from);
            }
        }
        steps += 1;
    }
    let mut moved = Vec::new();
    for e in entries.iter_mut() {
        let Some(nid) = text_at(e, "id") else {
            continue;
        };
        if !kids.iter().any(|k| k == nid) {
            continue;
        }
        let nid = nid.to_string();
        e.as_object_mut().expect("row is an object").insert(
            "parent".into(),
            target.clone().map(Value::String).unwrap_or(Value::Null),
        );
        moved.push((nid, target.clone()));
    }
    moved
}

fn reparent_receipt(pairs: &[(String, Option<String>)]) -> String {
    let listed: Vec<String> = pairs
        .iter()
        .map(|(cid, p)| format!("{cid} -> {}", p.clone().unwrap_or_else(|| "(none)".into())))
        .collect();
    format!(
        "re-parented {} stranded child(ren) under terminal parents: {}",
        pairs.len(),
        listed.join(", ")
    )
}

fn parse_iso(value: Option<&Value>) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    let raw = value.and_then(Value::as_str)?;
    if raw.trim().is_empty() {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(raw).ok()
}

/// Every child closed, and at least one really shipped (the
/// children_all_closed twin). A child superseded BY this parent never counts
/// closed against it.
fn children_all_closed(parent: &Value, kids: &[&Value]) -> bool {
    let pid = text_at(parent, "id").unwrap_or("");
    if kids.is_empty() {
        return false;
    }
    for k in kids {
        if !is_live(k) || text_at(k, "superseded_by") == Some(pid) {
            continue;
        }
        return false;
    }
    kids.iter().any(|k| {
        text_at(k, "status") != Some("superseded") && text_at(k, "superseded_by").is_none()
    })
}

/// True when a deliberate reopen postdates every child's close (the
/// _reopen_outranks_child_closes twin). Ambiguity favours the human.
fn reopen_outranks_child_closes(parent: &Value, kids: &[&Value]) -> bool {
    let reopened = match parse_iso(parent.get("reopened_at")) {
        Some(t) => t,
        None => {
            let raw = text_at(parent, "reopened_at").unwrap_or("");
            return !raw.trim().is_empty();
        }
    };
    for kid in kids {
        if let Some(closed) = parse_iso(kid.get("completed_at")) {
            if closed >= reopened {
                return false;
            }
        }
    }
    true
}

fn clear_reopen_warning_if_child_matches(parent: &mut Value, child_id: Option<&str>) {
    let matches = parent
        .get("reopen_warning")
        .filter(|m| m.is_object())
        .and_then(|m| m.get("child"))
        .and_then(Value::as_str)
        .zip(child_id)
        .map(|(a, b)| a == b)
        .unwrap_or(false);
    if matches {
        parent
            .as_object_mut()
            .expect("row is an object")
            .remove("reopen_warning");
    }
}

fn auto_closed_note(entry: &Value) -> String {
    let has_plan = truthy_field(entry, "plan_path")
        || entry
            .get("plan_path")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
    let has_pr = entry.get("pr_number").and_then(Value::as_u64).is_some();
    if has_plan && !has_pr {
        return "auto-closed: all children complete; own plan deliverables UNVERIFIED (plan_path set, no PR)".into();
    }
    "auto-closed: all children complete".into()
}

/// Set the fields that mark a row done (the _apply_completion_fields twin).
/// `merge_status` is stamped only when a caller resolved MERGED from gh.
fn apply_completion_fields(node: &mut Value, merge_status: bool) {
    let obj = node.as_object_mut().expect("row is an object");
    for key in [
        "locked_by",
        "locked_at",
        "deferred_at",
        "deferred_reason",
        "queued_at",
        "queued_reason",
    ] {
        obj.insert(key.into(), Value::Null);
    }
    obj.remove("deferred_kind");
    obj.insert("status".into(), Value::String("done".into()));
    obj.insert(
        "completed_at".into(),
        Value::String(graph_store::now_isoformat()),
    );
    if merge_status {
        obj.insert("merge_status".into(), Value::String("merged".into()));
    }
}

/// Undo a close (the _clear_completion_fields twin). `status` is the
/// plan-rung ladder's answer for this row, computed by the caller.
fn clear_completion_fields(node: &mut Value, reason: &str, status: &str) {
    let obj = node.as_object_mut().expect("row is an object");
    obj.insert("completed_at".into(), Value::Null);
    obj.insert("completion_note".into(), Value::Null);
    obj.insert(
        "reopened_at".into(),
        Value::String(graph_store::now_isoformat()),
    );
    obj.insert("reopened_reason".into(), Value::String(reason.into()));
    obj.remove("reopen_warning");
    obj.insert("status".into(), Value::String(status.into()));
}

/// Close ancestor epics whose children are now all complete (the
/// _cascade_close_parents twin). Returns the closed ancestor ids.
fn cascade_close_parents(entries: &mut [Value], node_id: &str) -> Vec<String> {
    let id_of = |e: &Value| text_at(e, "id").map(String::from);
    let mut closed: Vec<String> = Vec::new();
    let mut cur: Option<String> = entries
        .iter()
        .find(|e| text_at(e, "id") == Some(node_id))
        .and_then(|e| text_at(e, "parent").map(String::from));
    for _ in 0..64 {
        let Some(pid) = cur.clone() else { break };
        let idx = match entries
            .iter()
            .position(|e| id_of(e).as_deref() == Some(pid.as_str()))
        {
            Some(i) => i,
            None => break,
        };
        // A parent with live children, an outranking reopen, or an already
        // done row stops the walk.
        let kids: Vec<Value> = entries
            .iter()
            .filter(|e| text_at(e, "parent") == Some(pid.as_str()))
            .cloned()
            .collect();
        let kid_refs: Vec<&Value> = kids.iter().collect();
        let parent_is_done = truthy_field(&entries[idx], "completed_at");
        let outranks = reopen_outranks_child_closes(&entries[idx], &kid_refs);
        let all_closed = children_all_closed(&entries[idx], &kid_refs);
        if parent_is_done || outranks || !all_closed {
            clear_reopen_warning_if_child_matches(&mut entries[idx], Some(node_id));
            break;
        }
        clear_reopen_warning_if_child_matches(&mut entries[idx], Some(node_id));
        apply_completion_fields(&mut entries[idx], false);
        let note = auto_closed_note(&entries[idx]);
        let obj = entries[idx].as_object_mut().expect("row is an object");
        if !obj
            .get("completion_note")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
        {
            obj.insert("completion_note".into(), Value::String(note));
        }
        obj.remove("mission_active");
        closed.push(pid.clone());
        cur = obj.get("parent").and_then(Value::as_str).map(String::from);
    }
    closed
}

// ---------------------------------------------------------------------------
// events, drive audit, retro, plan tail
// ---------------------------------------------------------------------------

fn emit_event(name: &str, data: Value) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    let Some(space) = crate::paths::space_dir_opt(&cwd) else {
        return;
    };
    let path = space.join("events.jsonl");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let emitter = crate::events::EventEmitter::new(path, "backlog");
    let _ = emitter.emit(name, &data);
}

/// Best-effort operator audit during a drive window: an informational action
/// (`backlog done`) is allowed but attributed (the active_drive_sessions twin).
fn drive_audit(task_id: &str) {
    let base = match settings::state_dir() {
        Some(dir) => dir.join("agents"),
        None => return,
    };
    let Ok(entries) = std::fs::read_dir(&base) else {
        return;
    };
    let mut children: Vec<_> = entries.flatten().collect();
    children.sort_by_key(|c| c.file_name());
    for child in children {
        let p = child.path();
        if !p.is_dir() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(p.join("state.json")) else {
            continue;
        };
        let Ok(data) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(pty) = data.get("pty").filter(|p| p.is_object()) else {
            continue;
        };
        if !truthy_field(pty, "drive_active") {
            continue;
        }
        let mode = text_at(pty, "drive_mode").unwrap_or("");
        if matches!(mode, "interactive" | "step" | "paranoid") {
            emit_event(
                "backlog_done_operator_initiated",
                json!({"source": "backlog", "task_id": task_id}),
            );
            return;
        }
    }
}

/// The A2 retro-at-done trigger, gated by config.think_spawn.on_retro
/// (default OFF). When on, the Python lifecycle implementation dispatches
/// the think spawn; a dispatch failure never unwinds the close.
fn retro_trigger(node: &Value) {
    let on = settings::config_candidates().iter().any(|doc| {
        doc.get("think_spawn")
            .and_then(|t| t.get("on_retro"))
            .map(|v| !v.is_null() && v.as_bool() != Some(false))
            .unwrap_or(false)
    });
    if !on {
        return;
    }
    let payload = serde_json::to_string(node).unwrap_or_default();
    let _ = std::process::Command::new(crate::scrape::fno_py())
        .args([
            "-c",
            "import json,sys;from fno.provenance.spawn_think import on_node_retro;on_node_retro(json.load(sys.stdin))",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            if let Some(mut stdin) = c.stdin.take() {
                let _ = stdin.write_all(payload.as_bytes());
            }
            c.wait()
        });
}

/// Holder-agnostic release of the `node:<id>` claim a closure just made
/// moot: the closer is usually not the worker holding it, the terminal rung
/// is the authority, and the reaper is the backstop. Best-effort and loud.
fn release_claim_at_closure(node_id: &str) {
    let key = format!("node:{node_id}");
    let mut roots: Vec<Option<std::path::PathBuf>> =
        vec![crate::claims::claims_root_for(&key), None];
    roots.dedup();
    for root in roots {
        let Ok(path) = crate::claims::claim_path(&key, root.as_deref()) else {
            continue;
        };
        if !path.exists() {
            continue;
        }
        if let Err(e) = std::fs::remove_file(&path) {
            eprintln!("node closure: claim release failed for {key}: {e}");
        }
    }
}

/// Stamp a plan `shipped` against the evidencing PR then graduate (the
/// _stamp_and_graduate_plan twin). Best-effort; the close never fails on it.
fn stamp_and_graduate_plan(plan_path: &str, url: Option<&str>, session_id: Option<&str>) {
    let path = Path::new(plan_path);
    if let Some(url) = url {
        let sid = session_id.unwrap_or("backlog-close");
        crate::plan_doc::stamp::cmd_stamp(path, sid, &[url.to_string()], None, false, None);
    }
    crate::plan_doc::stamp::cmd_graduate(path, false, None);
}

/// Project the closed node and its cascade-closed epic parents onto their
/// plans (the _project_plans_from_graph twin). Best-effort per node.
fn project_plans(graph: &Path, node_ids: &[String]) {
    let Ok(entries) = graph_store::read_rows(graph) else {
        return;
    };
    let _ =
        crate::plan_doc::project::project_graph_nodes(&entries, node_ids, None, None, None, None);
}

/// The shared post-close tail: plan stamp, projection, retro trigger.
fn canonical_post_close(
    graph: &Path,
    task_id: &str,
    plan_path: Option<&str>,
    session_id: Option<&str>,
    cascade_closed: &[String],
    skip_stamp: bool,
    evidence_pr_url: Option<&str>,
) {
    if let Some(plan) = plan_path {
        if !skip_stamp {
            stamp_and_graduate_plan(plan, evidence_pr_url, session_id);
        }
    }
    if !skip_stamp {
        let mut ids = vec![task_id.to_string()];
        ids.extend(cascade_closed.iter().cloned());
        project_plans(graph, &ids);
    }
    retro_trigger(&retro_node_stub(task_id, plan_path));
}

// The retro trigger takes the closed node's row; after the commit the row's
// completed_at is what the python caller passed. Rebuilt from the stored
// fields the spawn actually reads.
fn retro_node_stub(task_id: &str, plan_path: Option<&str>) -> Value {
    let mut n = json!({"id": task_id});
    if let Some(p) = plan_path {
        n.as_object_mut()
            .unwrap()
            .insert("plan_path".into(), Value::String(p.into()));
    }
    n
}

// ---------------------------------------------------------------------------
// the close verb
// ---------------------------------------------------------------------------

/// `fno backlog done` — the close-flag surface. Rich completion shapes ride
/// the Python forward; so does `--help` and every parse error.
pub fn run_done(tail: &[String]) -> i32 {
    if tail.is_empty() || tail.iter().any(|a| a == "--help" || a == "-h") {
        return forward_to_python("done", tail);
    }
    let args = parse_done_args(tail);
    let close_flags = args.close_flags();
    let rich_flags = args.rich_flags();
    if close_flags && rich_flags {
        eprintln!(
            "Error: the close flags (--force/--reason/--skip-stamp) and the \
             completion flags (--pr/--pr-url/--link/--note/--backfill/--force-overwrite) \
             are separate paths. A deliberate half-ship closes on the id alone: \
             `fno backlog done <id> --force --reason ...`"
        );
        return 2;
    }
    let delegate = !close_flags
        && (rich_flags
            || args.id.is_none()
            || args.id.as_deref().map(has_node_id_prefix) != Some(true));
    if delegate || args.unknown {
        return forward_to_python("done", tail);
    }
    let Some(task_id) = args.id.clone() else {
        eprintln!(
            "Error: an explicit node id is required with the close flags \
             (--force/--reason/--skip-stamp); branch auto-detect applies to \
             the completion surface only"
        );
        return 2;
    };
    close_node(tail, &args, &task_id)
}

fn close_node(tail: &[String], args: &DoneArgs, task_id: &str) -> i32 {
    // External backend: the shared gates then exactly one tracker.close are
    // the seam's job (still Python-owned).
    if external_backend_selected() {
        return forward_to_python("done", tail);
    }
    if !has_node_id_prefix(task_id) {
        eprintln!("Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'");
        return 1;
    }
    if args.force && args.reason.is_none() {
        eprintln!(
            "Error: --force requires --reason TEXT (explain why the cross-check is bypassed)"
        );
        return 2;
    }
    let graph = settings::graph_path();
    let Ok(rows) = graph_store::read_rows(&graph) else {
        eprintln!("Error: the backlog graph could not be read");
        return 1;
    };
    let Some(node) = find_node(&rows, task_id) else {
        eprintln!("Error: feature {task_id} not found");
        return 1;
    };
    // The store's row space includes imported archive rows; a direct wheel
    // spelling answered "not found" for those, so the close keeps that
    // contract instead of closing a row that is not on the board.
    if truthy_field(node, "archived_at") {
        eprintln!("Error: feature {task_id} not found");
        return 1;
    }
    // The CANONICAL id, not the argument: the partial-id spellings resolve
    // here, and the cascade and receipts must walk the full id.
    let task_id = text_at(node, "id")
        .map(String::from)
        .unwrap_or_else(|| task_id.to_string());
    let task_id = task_id.as_str();
    if node
        .get("completed_at")
        .map(|v| !v.is_null())
        .unwrap_or(false)
    {
        eprintln!("{task_id} is already done");
        return 0;
    }

    let cwd = text_at(&node, "cwd").map(String::from);
    let refs = node_pr_refs(&node);
    let mut evidence_pr_url: Option<String> = None;
    if !refs.is_empty() && !args.force {
        let evidence = resolve_merge_evidence(&refs, cwd.as_deref());
        match evidence.outcome {
            Outcome::Merged => evidence_pr_url = evidence.pr_url.clone(),
            Outcome::AwaitingMerge => {
                let note = evidence
                    .error
                    .as_ref()
                    .map(|e| format!(" (note: {e})"))
                    .unwrap_or_default();
                eprintln!(
                    "awaiting merge: PR #{} is OPEN, not merged. {task_id} stays in_review \
                     and closes on merge (reconcile / merge-triggered advance). \
                     Use --force --reason TEXT for an early close.{note}",
                    evidence.open_pr_number.unwrap_or_default(),
                );
                return 5;
            }
            Outcome::Outage => {
                eprintln!(
                    "{}",
                    render_merge_evidence_failure(task_id, &evidence, "open")
                );
                return evidence.exit_code();
            }
            Outcome::Refused => {
                let msg = evidence
                    .reason
                    .clone()
                    .unwrap_or_else(|| format!("PR #{}: no merged evidence", refs[0].0));
                if evidence.remedy.is_some() {
                    eprintln!(
                        "{}",
                        render_merge_evidence_failure(task_id, &evidence, "open")
                    );
                } else {
                    eprintln!(
                        "Refused: {task_id} cross-check failed: {msg}\nUse --force --reason TEXT to bypass."
                    );
                }
                emit_event(
                    "backlog_done_refused",
                    json!({"node_id": task_id, "pr_number": refs[0].0, "reason": msg}),
                );
                return evidence.exit_code();
            }
        }
    }
    if args.force && !refs.is_empty() {
        let (first_number, first_url) = &refs[0];
        evidence_pr_url = first_url.clone();
        let pr_repo = repo_slug_from_url(first_url.as_deref());
        let state = query_pr_state(*first_number, pr_repo.as_deref(), cwd.as_deref())
            .map(|(s, _)| s)
            .unwrap_or_else(|_| "UNKNOWN".into());
        let reason = args.reason.clone().unwrap_or_default();
        eprintln!(
            "Warning: force-closing {task_id} (reason: {reason}). PR #{first_number} state={state}."
        );
        emit_event(
            "backlog_done_forced",
            json!({
                "node_id": task_id,
                "force_reason": reason,
                "pr_number": first_number,
                "pr_state": state,
            }),
        );
    } else if args.force {
        let reason = args.reason.clone().unwrap_or_default();
        eprintln!(
            "Warning: force flag set on advisory node {task_id} (reason: {reason}); no PR refs to check."
        );
    }
    if !args.force {
        let promise = resolve_promise_evidence(&node, cwd.as_deref(), &[]);
        if !promise.satisfied() {
            if let Some(reason) = &promise.reason {
                eprintln!("{reason}");
            }
            return promise.exit_code();
        }
        if let Some(warning) = &promise.warning {
            eprintln!("warning: {warning}");
        }
    }

    // Cost stamp: the ledger's per-plan cost the node never captured.
    let rollup = rollup_from_ledger(&node);
    let env_session = std::env::var("CLAUDECODE_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty());

    // Mutation under the store lock.
    let not_found = RefCell::new(false);
    let raced = RefCell::new(false);
    let live_kids: RefCell<Option<Vec<String>>> = RefCell::new(None);
    let reparented: RefCell<Vec<(String, Option<String>)>> = RefCell::new(Vec::new());
    let cascade: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let session_after: RefCell<Option<String>> = RefCell::new(None);
    let plan_after: RefCell<Option<String>> = RefCell::new(None);
    let force = args.force;
    let evidence_present = evidence_pr_url.is_some();
    let task = task_id.to_string();
    let applied = mutate_single_row(&graph, "backlog_done", |rows| {
        let Some(idx) = rows
            .iter()
            .position(|e| text_at(e, "id").map(String::from).as_deref() == Some(task.as_str()))
        else {
            not_found.replace(true);
            return Ok(false);
        };
        if truthy_field(&rows[idx], "completed_at") {
            raced.replace(true);
            return Ok(false);
        }
        let kids = live_child_ids(rows, &task);
        if !kids.is_empty() && !force {
            live_kids.replace(Some(kids));
            return Ok(false);
        }
        if !kids.is_empty() {
            let moved = reparent_live_children(rows, &task);
            reparented.replace(moved);
        }
        apply_completion_fields(&mut rows[idx], evidence_present);
        let obj = rows[idx].as_object_mut().expect("row is an object");
        if rollup.cost_usd.is_some() && obj.get("cost_usd").and_then(Value::as_f64).is_none() {
            obj.insert("cost_usd".into(), json!(rollup.cost_usd));
        }
        if !rollup.cost_sessions.is_empty()
            && obj
                .get("cost_sessions")
                .and_then(Value::as_array)
                .map(|a| a.is_empty())
                .unwrap_or(true)
        {
            obj.insert(
                "cost_sessions".into(),
                Value::Array(rollup.cost_sessions.clone()),
            );
        }
        let existing = obj
            .get("session_id")
            .and_then(Value::as_str)
            .map(String::from);
        let sid = existing
            .clone()
            .or_else(|| env_session.clone())
            .or_else(|| rollup.session_id.clone());
        if let Some(sid) = sid {
            if existing.is_none() {
                obj.insert("session_id".into(), Value::String(sid.clone()));
            }
            // The plan stamp records the row's session first (the python
            // twin stamped the node's own session, not the caller's).
            session_after.replace(Some(sid));
        }
        if let Some(points) = &rollup.points {
            if obj.get("points").map(Value::is_null).unwrap_or(true) {
                obj.insert("points".into(), points.clone());
            }
        }
        if obj
            .get("plan_path")
            .and_then(Value::as_str)
            .map(String::from)
            .is_some()
        {
            plan_after.replace(
                obj.get("plan_path")
                    .and_then(Value::as_str)
                    .map(String::from),
            );
        }
        let closed = cascade_close_parents(rows, &task);
        cascade.replace(closed);
        Ok(true)
    });
    if applied.is_err() {
        let err = applied.unwrap_err();
        eprintln!("Error: {err}");
        return 1;
    }
    if not_found.into_inner() {
        eprintln!("Error: feature {task_id} not found");
        return 1;
    }
    if raced.into_inner() {
        eprintln!("{task_id} is already done");
        return 0;
    }
    if let Some(kids) = live_kids.into_inner() {
        eprintln!(
            "Error: cannot close {task_id}: it still has {} live child(ren): {}. \
             Closing would strand them under a terminal parent. Re-run with \
             --force --reason to close anyway (each child is re-parented to its \
             nearest live ancestor).",
            kids.len(),
            kids.join(", "),
        );
        return 1;
    }
    let cascade_closed = cascade.into_inner();
    let moved = reparented.into_inner();
    println!("Marked {task_id} done");
    if !moved.is_empty() {
        println!("{}", reparent_receipt(&moved));
    }
    drive_audit(task_id);
    canonical_post_close(
        &graph,
        task_id,
        plan_after.into_inner().as_deref(),
        session_after.into_inner().as_deref(),
        &cascade_closed,
        args.skip_stamp,
        evidence_pr_url.as_deref(),
    );
    for id in std::iter::once(task_id).chain(cascade_closed.iter().map(String::as_str)) {
        release_claim_at_closure(id);
    }
    0
}

// ---------------------------------------------------------------------------
// the reopen verb
// ---------------------------------------------------------------------------

/// `fno backlog reopen` — clears a node's completion. Fully native: the
/// Python twin and its registry row are gone.
pub fn run_reopen(tail: &[String]) -> i32 {
    let (id, reason, force, help, unknown) = parse_reopen_args(tail);
    if help {
        // The verb's own help is the Python surface's rendering, retired with
        // the twin; a bare usage line keeps the shape discoverable.
        println!("fno backlog reopen <id> --reason TEXT [--force]");
        return 0;
    }
    if unknown {
        println!("fno backlog reopen <id> --reason TEXT [--force]");
        return 2;
    }
    let backend = active_backend_name();
    if backend != "graph" {
        eprintln!(
            "fno backlog reopen: this verb owns graph state; under the {backend} \
             tracker backend it is refused. Track the item in the tracker by its id."
        );
        return 1;
    }
    let Some(task_id) = id else {
        eprintln!("Missing option '--reason' / '-R'.");
        eprintln!("Usage: fno backlog reopen <id> --reason TEXT [--force]");
        return 2;
    };
    let Some(reason) = reason else {
        eprintln!("Missing option '--reason' / '-R'.");
        eprintln!("Usage: fno backlog reopen <id> --reason TEXT [--force]");
        return 2;
    };
    if !has_node_id_prefix(&task_id) {
        eprintln!("Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'");
        return 1;
    }
    let cleaned_reason = reason.trim().to_string();
    if cleaned_reason.is_empty() {
        eprintln!("Error: --reason cannot be blank");
        return 2;
    }
    let graph = settings::graph_path();
    let Ok(rows) = graph_store::read_rows(&graph) else {
        eprintln!("Error: the backlog graph could not be read");
        return 1;
    };
    let Some(node) = find_node(&rows, &task_id) else {
        if let Some(archived) = archived_entry(&task_id) {
            let when = archived
                .get("completed_at")
                .and_then(Value::as_str)
                .or_else(|| archived.get("updated").and_then(Value::as_str))
                .unwrap_or("unknown");
            eprintln!(
                "Refused: {task_id} is archived (terminal since {when}), not in the \
                 working graph. Run `fno backlog unarchive {task_id}` first, then reopen it."
            );
            return 4;
        }
        eprintln!("Error: feature {task_id} not found");
        return 1;
    };
    // The store's row space includes imported archive rows: an archived row
    // resolves through the same lookup, so the refusal must fire before the
    // not-done check, not only on a miss.
    if truthy_field(&node, "archived_at") {
        let when = text_at(&node, "completed_at")
            .or_else(|| text_at(&node, "updated"))
            .unwrap_or("unknown");
        eprintln!(
            "Refused: {task_id} is archived (terminal since {when}), not in the \
             working graph. Run `fno backlog unarchive {task_id}` first, then reopen it."
        );
        return 4;
    }
    // The CANONICAL id, not the argument: a partial-id spelling resolves
    // here, and the cascade walks full ids.
    let task_id: String = text_at(node, "id").map(String::from).unwrap_or(task_id);
    if !truthy_field(&node, "completed_at") {
        eprintln!("warning: {task_id} is not done; nothing to reopen");
        return 0;
    }

    // The gate is done's inverse: a MERGED ref refuses unless forced.
    let cwd = text_at(&node, "cwd").map(String::from);
    let refs = node_pr_refs(&node);
    let mut pr_number: Option<i64> = None;
    let mut pr_state: Option<String> = None;
    let mut bypassed_merged = false;
    if !refs.is_empty() {
        let evidence = resolve_merge_evidence(&refs, cwd.as_deref());
        pr_number = evidence_pr_number(&evidence, &refs);
        if evidence.outcome == Outcome::Outage && !force {
            eprintln!(
                "{}",
                render_merge_evidence_failure(&task_id, &evidence, "done")
            );
            return 4;
        }
        if evidence.outcome == Outcome::Merged {
            pr_state = Some("MERGED".into());
            let merged_url = evidence.pr_url.clone().unwrap_or_default();
            if !force {
                eprintln!(
                    "Refused: a referenced PR is MERGED, so {task_id}'s work is in main{}. \
                     Reopening would make the graph assert that shipped work did not ship.\n\
                     \x20 If work remains, file it: fno backlog idea \"<what is left>\"\n\
                     \x20 If the close itself was wrong: reopen --force --reason \"...\"",
                    if merged_url.is_empty() {
                        String::new()
                    } else {
                        format!(" ({merged_url})")
                    },
                );
                return 3;
            }
            bypassed_merged = true;
            let cleaned = cleaned_reason.clone();
            eprintln!(
                "Warning: force-reopening {task_id} (reason: {cleaned}). A referenced PR is MERGED{}.",
                if merged_url.is_empty() {
                    String::new()
                } else {
                    format!(" ({merged_url})")
                },
            );
        } else if evidence.outcome == Outcome::AwaitingMerge {
            pr_state = Some("OPEN".into());
        } else if evidence.failure_kind.is_some() {
            eprintln!(
                "{}",
                render_merge_evidence_failure(&task_id, &evidence, "done")
            );
            return evidence.exit_code();
        } else {
            pr_state = Some("UNKNOWN".into());
        }
    }

    // The reopen statuses are the plan-rung ladder's answers; repo law keeps
    // plan-document reading on the Python side, so the answers cross as data.
    let rungs = reopen_statuses(&rows, &task_id);

    let raced = RefCell::new(false);
    let not_found = RefCell::new(false);
    let canonical: RefCell<Option<String>> = RefCell::new(None);
    let cascade: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let warned: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let reason_text = cleaned_reason.clone();
    let applied = mutate_single_row(&graph, "backlog_reopen", |rows| {
        let Some(idx) = rows
            .iter()
            .position(|e| text_at(e, "id").map(String::from).as_deref() == Some(task_id.as_str()))
        else {
            not_found.replace(true);
            return Ok(false);
        };
        if !truthy_field(&rows[idx], "completed_at") {
            raced.replace(true);
            return Ok(false);
        }
        let canonical_id = text_at(&rows[idx], "id")
            .map(String::from)
            .unwrap_or_else(|| task_id.clone());
        {
            let status = rungs
                .get(&canonical_id)
                .cloned()
                .unwrap_or_else(|| "idea".into());
            clear_completion_fields(&mut rows[idx], &reason_text, &status);
        }
        let (reopened, warned_ids) = cascade_reopen_parents(rows, &canonical_id, &task_id, &rungs);
        cascade.replace(reopened);
        warned.replace(warned_ids);
        canonical.replace(Some(canonical_id));
        Ok(true)
    });
    if applied.is_err() {
        let err = applied.unwrap_err();
        eprintln!("Error: {err}");
        return 1;
    }
    if not_found.into_inner() {
        eprintln!("Error: feature {task_id} not found");
        return 1;
    }
    if raced.into_inner() {
        eprintln!("warning: {task_id} was reopened by another writer; nothing to do");
        return 0;
    }
    let cascade_out = cascade.into_inner();
    let canonical_id = canonical.into_inner().unwrap_or_else(|| task_id.clone());
    for pid in warned.into_inner() {
        println!(
            "warning: parent {pid} is done on its own evidence and now has an open child; \
             `fno backlog reopen {pid} --reason \"...\"` if that is wrong"
        );
    }
    if cascade_out.is_empty() {
        println!("Reopened {canonical_id}");
    } else {
        println!(
            "Reopened {canonical_id} (cascade: {})",
            cascade_out.join(", ")
        );
    }
    emit_event(
        "backlog_reopened",
        json!({
            "node_id": canonical_id,
            "reason": cleaned_reason,
            "forced": bypassed_merged,
            "pr_number": pr_number,
            "pr_state": pr_state,
            "cascade_reopened": cascade_out,
        }),
    );
    // Force the plan doc off terminal `done`, then recompute the derived
    // status through one no-op write (the projector is forward-only).
    let mut ids = vec![canonical_id.clone()];
    ids.extend(cascade_out.iter().cloned());
    for nid in &ids {
        project_one_off_terminal(&graph, nid);
    }
    let _ = mutate_single_row(&graph, "backlog_reopen_recompute", |rows| {
        Ok(!rows.is_empty())
    });
    0
}

fn project_one_off_terminal(graph: &Path, node_id: &str) {
    let Ok(entries) = graph_store::read_rows(graph) else {
        return;
    };
    let _ = crate::plan_doc::project::project_graph_nodes(
        &entries,
        std::slice::from_ref(&node_id.to_string()),
        None,
        None,
        Some(node_id.to_string()),
        None,
    );
}

/// The PR the evidence came from, not merely the first ref (the
/// _evidence_pr_number twin).
fn evidence_pr_number(
    evidence: &super::merge_evidence::MergeEvidence,
    refs: &[(i64, Option<String>)],
) -> Option<i64> {
    if evidence.outcome == Outcome::Merged {
        if let Some(url) = &evidence.pr_url {
            for (number, u) in refs {
                if u.as_deref() == Some(url.as_str()) {
                    return Some(*number);
                }
            }
        }
    }
    if evidence.outcome == Outcome::AwaitingMerge {
        return evidence.open_pr_number;
    }
    refs.first().map(|(n, _)| *n)
}

/// Reopen ancestor epics the cascade auto-closed (the
/// _cascade_reopen_parents twin): auto-closed ancestors clear, done-on-own-
/// evidence ancestors get the reopen-warning marker and stop the climb.
fn cascade_reopen_parents(
    entries: &mut [Value],
    node_id: &str,
    reason_source: &str,
    rungs: &std::collections::HashMap<String, String>,
) -> (Vec<String>, Vec<String>) {
    let mut reopened: Vec<String> = Vec::new();
    let mut warned: Vec<String> = Vec::new();
    let mut cur: Option<String> = entries
        .iter()
        .find(|e| text_at(e, "id") == Some(node_id))
        .and_then(|e| text_at(e, "parent").map(String::from));
    for _ in 0..64 {
        let Some(pid) = cur.clone() else { break };
        let idx = match entries
            .iter()
            .position(|e| text_at(e, "id").map(String::from).as_deref() == Some(pid.as_str()))
        {
            Some(i) => i,
            None => break,
        };
        if !truthy_field(&entries[idx], "completed_at") {
            break;
        }
        let note = text_at(&entries[idx], "completion_note").unwrap_or("");
        if !note.starts_with("auto-closed:") {
            warned.push(pid.clone());
            let child_id = reopened
                .last()
                .cloned()
                .unwrap_or_else(|| node_id.to_string());
            let obj = entries[idx].as_object_mut().expect("row is an object");
            obj.insert(
                "reopen_warning".into(),
                json!({"child": child_id, "at": graph_store::now_isoformat()}),
            );
            let _ = reason_source;
            break;
        }
        let status = rungs.get(&pid).cloned().unwrap_or_else(|| "idea".into());
        clear_completion_fields(
            &mut entries[idx],
            &format!("child {node_id} reopened"),
            &status,
        );
        reopened.push(pid.clone());
        cur = entries[idx]
            .get("parent")
            .and_then(Value::as_str)
            .map(String::from);
    }
    (reopened, warned)
}

/// The reopen status per row: the ladder's idea-vs-ready answer. A row with
/// no plan is the NONE rung; a plan's rung comes from the sanctioned Python
/// classifier, shelled once per distinct plan before the lock.
fn reopen_statuses(rows: &[Value], node_id: &str) -> std::collections::HashMap<String, String> {
    let mut rungs = std::collections::HashMap::new();
    let mut cur: Option<String> = Some(node_id.to_string());
    let mut seen: Vec<String> = Vec::new();
    let mut cache: std::collections::HashMap<String, String> = Default::default();
    while let Some(id) = cur.clone() {
        if id.is_empty() || seen.contains(&id) {
            break;
        }
        seen.push(id.clone());
        let Some(row) = rows.iter().find(|e| text_at(e, "id") == Some(id.as_str())) else {
            break;
        };
        let status = match text_at(row, "plan_path") {
            None => "idea".to_string(),
            Some(plan) => {
                let answer = cache
                    .entry(plan.to_string())
                    .or_insert_with(|| plan_rung_status(plan, text_at(row, "cwd")));
                answer.clone()
            }
        };
        rungs.insert(id, status);
        cur = text_at(row, "parent").map(String::from);
    }
    rungs
}

/// One `fno do plan rung` round-trip: exit 0 = dispatchable (a non-idea
/// rung), anything else degrades to `idea` the way the ladder's NONE rung
/// does for an unreadable plan.
fn plan_rung_status(plan_path: &str, cwd: Option<&str>) -> String {
    let mut path = std::path::PathBuf::from(plan_path.split('#').next().unwrap_or(plan_path));
    if !path.is_absolute() {
        if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
            path = Path::new(cwd).join(path);
        }
    }
    let Ok(out) = std::process::Command::new(crate::scrape::fno_py())
        .args(["do", "plan", "rung"])
        .arg(&path)
        .output()
    else {
        return "idea".into();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if let Some(rung) = line.strip_prefix("rung=") {
            let rung = rung.trim().to_lowercase();
            return if matches!(rung.as_str(), "idea" | "none") {
                "idea".into()
            } else {
                "ready".into()
            };
        }
    }
    "idea".into()
}

fn archived_entry(node_id: &str) -> Option<Value> {
    let archive = settings::graph_path().parent()?.join("graph-archive.json");
    let read = graph_store::read_archive_raw(&archive).ok()?;
    let rows = match read {
        graph_store::RawRead::Entries(rows) => rows,
        _ => return None,
    };
    // The archive probe resolves a partial legacy id the way the working
    // graph does, or `reopen ab-2222` reports "not found" for a node sitting
    // readable in the archive.
    let exact = rows.iter().find(|e| text_at(e, "id") == Some(node_id));
    if exact.is_some() {
        return exact.cloned();
    }
    if node_id.starts_with("ab-") && node_id.len() < 11 {
        let matches: Vec<&Value> = rows
            .iter()
            .filter(|e| text_at(e, "id").is_some_and(|id| id.starts_with(node_id)))
            .collect();
        if matches.len() == 1 {
            return Some(matches[0].clone());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// small shared pieces
// ---------------------------------------------------------------------------

/// Exact match first; a legacy `ab-` partial resolves through unique prefix
/// matching the way the fuzzy resolver does (the _find_node twin). An
/// ambiguous prefix refuses with the candidate list.
fn find_node<'a>(rows: &'a [Value], task_id: &str) -> Option<&'a Value> {
    if let Some(found) = rows.iter().find(|e| text_at(e, "id") == Some(task_id)) {
        return Some(found);
    }
    if task_id.starts_with("ab-") && task_id.len() < 11 {
        let candidates: Vec<&Value> = rows
            .iter()
            .filter(|e| text_at(e, "id").is_some_and(|id| id.starts_with(task_id)))
            .collect();
        return match candidates.len() {
            1 => Some(candidates[0]),
            n if n > 1 => {
                let ids: Vec<&str> = candidates.iter().filter_map(|e| text_at(e, "id")).collect();
                eprintln!(
                    "[graph] ambiguous prefix '{task_id}' matches: {}",
                    ids.join(", ")
                );
                None
            }
            _ => None,
        };
    }
    None
}

fn external_backend_selected() -> bool {
    active_backend_name() != "graph"
}

fn active_backend_name() -> String {
    std::env::var("FNO_TRACKER_BACKEND")
        .ok()
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| "graph".into())
}

/// Aggregate session_id / cost_usd / cost_sessions / points from the ledger
/// (the _rollup_from_ledger twin). A contained node is suppressed empty
///-handed: the delivery unit carries the whole figure.
struct Rollup {
    session_id: Option<String>,
    cost_usd: Option<f64>,
    cost_sessions: Vec<Value>,
    points: Option<Value>,
}

fn rollup_from_ledger(node: &Value) -> Rollup {
    let empty = Rollup {
        session_id: None,
        cost_usd: None,
        cost_sessions: Vec::new(),
        points: None,
    };
    if !node.is_object() {
        return empty;
    }
    if text_at(node, "contained_in").is_some_and(|c| !c.is_empty()) {
        return empty;
    }
    let Some(plan_path) = text_at(node, "plan_path").filter(|p| !p.is_empty()) else {
        return empty;
    };
    let target = normalize_plan_path(plan_path);
    let Some(state_dir) = settings::state_dir() else {
        return empty;
    };
    let Ok(text) = std::fs::read_to_string(state_dir.join("ledger.json")) else {
        return empty;
    };
    let Ok(data) = serde_json::from_str::<Value>(&text) else {
        return empty;
    };
    let entries: Vec<Value> = if data.is_array() {
        data.as_array().unwrap().clone()
    } else if let Some(list) = data.get("entries").and_then(Value::as_array) {
        list.clone()
    } else {
        return empty;
    };
    let matching: Vec<&Value> = entries
        .iter()
        .filter(|e| {
            e.get("plan_path")
                .and_then(Value::as_str)
                .map(|p| normalize_plan_path(p) == target)
                .unwrap_or(false)
        })
        .collect();
    if matching.is_empty() {
        return empty;
    }
    let mut cost_sessions: Vec<Value> = Vec::new();
    for le in &matching {
        let sessions = le.get("sessions").and_then(Value::as_array);
        let cost = le.get("cost_usd").and_then(Value::as_f64).unwrap_or(0.0);
        let sid = le
            .get("fno_id")
            .and_then(Value::as_str)
            .or_else(|| le.get("session_id").and_then(Value::as_str))
            .or_else(|| sessions.and_then(|s| s.first()).and_then(Value::as_str));
        let mut row = serde_json::Map::new();
        row.insert(
            "session_id".into(),
            sid.map(|s| Value::String(s.to_string()))
                .unwrap_or(Value::Null),
        );
        row.insert("cost_usd".into(), json!((cost * 10000.0).round() / 10000.0));
        let stamp = le
            .get("completed")
            .and_then(Value::as_str)
            .or_else(|| le.get("started").and_then(Value::as_str));
        row.insert(
            "timestamp".into(),
            stamp
                .map(|s| Value::String(s.to_string()))
                .unwrap_or(Value::Null),
        );
        cost_sessions.push(Value::Object(row));
    }
    let sort_key = |le: &Value| -> String {
        le.get("completed")
            .and_then(Value::as_str)
            .or_else(|| le.get("started").and_then(Value::as_str))
            .unwrap_or("")
            .to_string()
    };
    let latest = matching
        .iter()
        .max_by_key(|le| sort_key(le))
        .expect("non-empty");
    let latest_sessions = latest.get("sessions").and_then(Value::as_array);
    let session_id = latest
        .get("fno_id")
        .and_then(Value::as_str)
        .or_else(|| latest.get("session_id").and_then(Value::as_str))
        .or_else(|| {
            latest_sessions
                .and_then(|s| s.last())
                .and_then(Value::as_str)
        })
        .map(String::from);
    let points = matching
        .iter()
        .find_map(|le| le.get("points").filter(|p| !p.is_null()).cloned());
    let total: f64 = cost_sessions
        .iter()
        .filter_map(|s| s.get("cost_usd").and_then(Value::as_f64))
        .sum();
    Rollup {
        session_id,
        cost_usd: if cost_sessions.is_empty() {
            None
        } else {
            Some((total * 10000.0).round() / 10000.0)
        },
        cost_sessions,
        points,
    }
}

fn normalize_plan_path(path: &str) -> String {
    path.trim()
        .split('#')
        .next()
        .unwrap_or("")
        .trim_end_matches('/')
        .to_string()
}

// The unused-cell guard: the canonical close reads its post-state through
// the RefCells above; this keeps the import list honest while the rich
// Python surface still owns MergeEvidence's unused constructor arms.
const _: () = ();

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn seed(id: &str, parent: Option<&str>) -> Value {
        let mut n = json!({"id": id, "title": id, "status": "in_progress", "priority": "p2"});
        if let Some(p) = parent {
            n.as_object_mut().unwrap().insert("parent".into(), json!(p));
        }
        n
    }

    #[test]
    fn done_parse_classifies_close_and_rich_flags() {
        let a = parse_done_args(&[
            "x-aaaa".into(),
            "--force".into(),
            "--reason".into(),
            "why".to_string(),
        ]);
        assert!(a.close_flags() && !a.rich_flags());
        let a = parse_done_args(&["x-aaaa".into(), "--note".into(), "done".to_string()]);
        assert!(!a.close_flags() && a.rich_flags());
        let a = parse_done_args(&["--force".into(), "--note".into(), "n".to_string()]);
        assert!(a.close_flags() && a.rich_flags());
    }

    #[test]
    fn live_children_exclude_contained_and_terminal() {
        let rows = vec![
            json!({"id": "ab-aaaaaaaa", "parent": "ab-cccccccc"}),
            json!({"id": "ab-bbbbbbbb", "parent": "ab-cccccccc", "contained_in": "ab-cccccccc"}),
            json!({"id": "ab-dddddddd", "parent": "ab-cccccccc", "completed_at": "2026-09-01T00:00:00+00:00"}),
        ];
        assert_eq!(
            live_child_ids(&rows, "ab-cccccccc"),
            vec!["ab-aaaaaaaa".to_string()]
        );
    }

    #[test]
    fn cascade_closes_an_ancestor_whose_children_all_closed() {
        let mut rows = vec![
            seed("ab-ffffffff", None),
            seed("ab-aaaaaaaa", Some("ab-ffffffff")),
            seed("ab-bbbbbbbb", Some("ab-ffffffff")),
        ];
        let idx = rows
            .iter()
            .position(|e| text_at(e, "id") == Some("ab-aaaaaaaa"))
            .unwrap();
        apply_completion_fields(&mut rows[idx], false);
        let idx = rows
            .iter()
            .position(|e| text_at(e, "id") == Some("ab-bbbbbbbb"))
            .unwrap();
        apply_completion_fields(&mut rows[idx], false);
        let closed = cascade_close_parents(&mut rows, "ab-bbbbbbbb");
        assert!(closed.contains(&"ab-ffffffff".to_string()), "{closed:?}");
        let epic = rows
            .iter()
            .find(|e| text_at(e, "id") == Some("ab-ffffffff"))
            .unwrap();
        assert_eq!(
            text_at(epic, "completion_note"),
            Some("auto-closed: all children complete")
        );
    }

    #[test]
    fn cascade_stops_on_a_live_sibling() {
        let mut rows = vec![
            seed("ab-ffffffff", None),
            seed("ab-aaaaaaaa", Some("ab-ffffffff")),
            seed("ab-bbbbbbbb", Some("ab-ffffffff")),
        ];
        let idx = rows
            .iter()
            .position(|e| text_at(e, "id") == Some("ab-aaaaaaaa"))
            .unwrap();
        apply_completion_fields(&mut rows[idx], false);
        let closed = cascade_close_parents(&mut rows, "ab-aaaaaaaa");
        assert!(closed.is_empty(), "{closed:?}");
    }

    #[test]
    fn a_reopen_postdating_child_closes_outranks_the_sweep() {
        let parent = json!({
            "id": "ab-ffffffff",
            "reopened_at": "2026-09-02T00:00:00+00:00",
        });
        let kids = vec![
            json!({"id": "ab-aaaaaaaa", "completed_at": "2026-09-01T00:00:00+00:00"}),
            json!({"id": "ab-bbbbbbbb", "completed_at": "2026-09-03T00:00:00+00:00"}),
        ];
        let refs: Vec<&Value> = kids.iter().collect();
        assert!(!reopen_outranks_child_closes(&parent, &refs));
        let older_kid = json!({"id": "ab-aaaaaaaa", "completed_at": "2026-08-01T00:00:00+00:00"});
        let refs: Vec<&Value> = vec![&older_kid];
        assert!(reopen_outranks_child_closes(&parent, &refs));
    }

    #[test]
    fn reparent_walks_to_the_nearest_live_ancestor() {
        let mut rows = vec![
            seed("ab-eeeeeeee", None),
            seed("ab-ffffffff", Some("ab-eeeeeeee")),
            seed("ab-aaaaaaaa", Some("ab-ffffffff")),
        ];
        let moved = reparent_live_children(&mut rows, "ab-ffffffff");
        assert_eq!(
            moved,
            vec![("ab-aaaaaaaa".to_string(), Some("ab-eeeeeeee".to_string()))]
        );
        let kid = rows
            .iter()
            .find(|e| text_at(e, "id") == Some("ab-aaaaaaaa"))
            .unwrap();
        assert_eq!(text_at(kid, "parent"), Some("ab-eeeeeeee"));
    }

    #[test]
    fn evidence_number_names_the_ref_that_merged() {
        let ev = super::super::merge_evidence::MergeEvidence {
            outcome: Outcome::Merged,
            pr_url: Some("https://github.com/o/r/pull/8".into()),
            open_pr_number: None,
            error: None,
            reason: None,
            failure_kind: None,
            remedy: None,
        };
        let refs = vec![
            (7i64, Some("https://github.com/o/r/pull/7".to_string())),
            (8i64, Some("https://github.com/o/r/pull/8".to_string())),
        ];
        assert_eq!(evidence_pr_number(&ev, &refs), Some(8));
    }

    #[test]
    fn normalize_strips_fragments_and_slashes() {
        assert_eq!(normalize_plan_path(" plans/p.md#wave-1 "), "plans/p.md");
        assert_eq!(normalize_plan_path("plans/p.md/"), "plans/p.md");
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    #[test]
    fn partial_prefix_finds_unique_row() {
        let rows = vec![
            json!({"id": "ab-eeeeeeee", "title": "e"}),
            json!({"id": "ab-cccccccc", "title": "c", "parent": "ab-eeeeeeee"}),
        ];
        let found = find_node(&rows, "ab-cccc");
        assert!(found.is_some(), "partial must resolve");
    }
}
