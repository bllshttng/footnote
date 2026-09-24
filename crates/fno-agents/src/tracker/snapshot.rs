//! The joined snapshot: one document for every backend, plus its last-good
//! cache.
//!
//! The Python `_build_live_snapshot` was the only builder and it dropped
//! `sessions` and `contained_in`; this builder joins the full sidecar. The
//! mux's board_reader keeps its last-good / stale-after-three-failures
//! machinery, and the door adds a last-good cache on disk so a backend
//! outage serves the last good read with a `stale_since` stamp instead of
//! blanking the board.

use super::{State, Tracker, TrackerError};
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashSet};

/// The stamp a closed-blocker tombstone carries: a projection of "this
/// dependency is satisfied", which is the only fact the consumer derives.
pub(crate) const CLOSED_STAMP: &str = "closed";

/// The closed window the snapshot serves: the Done column and cycle time.
const CLOSED_WINDOW_DAYS: u32 = 14;

/// The graph-get door's `snapshot` op. On a failed build with `stale_ok`, it
/// answers the cached last-good snapshot plus `stale_since` and an errors
/// line naming the failure; without `stale_ok`, or with no cache for this
/// backend-plus-scope, it answers `{"error": ...}`.
pub fn door_snapshot(t: &dyn Tracker, backend: &str, stale_ok: bool) -> Value {
    let scope = scope_for(backend);
    match build(t, backend) {
        Ok(doc) => {
            cache_write(backend, &scope, &doc);
            doc
        }
        Err(e) => {
            let err_line = e.to_string();
            if stale_ok {
                if let Some((taken_at, mut doc)) = cache_read(backend, &scope) {
                    if let Some(obj) = doc.as_object_mut() {
                        let errors = obj
                            .entry("errors")
                            .or_insert_with(|| Value::Array(Vec::new()));
                        if let Some(arr) = errors.as_array_mut() {
                            arr.push(Value::String(err_line.clone()));
                        }
                    }
                    if let Some(obj) = doc.as_object_mut() {
                        obj.insert("stale_since".into(), Value::String(taken_at));
                    }
                    return doc;
                }
            }
            json!({ "error": err_line })
        }
    }
}

/// What selects the item set besides the backend name: the GitHub repo for
/// github, empty for graph; each later backend names its own (the Linear
/// team, the Jira project). A repo switch never serves another repo's
/// cached issues as stale data.
fn scope_for(backend: &str) -> String {
    match backend {
        "github" => std::env::var("FNO_TRACKER_GITHUB_REPO").unwrap_or_default(),
        _ => String::new(),
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn cache_dir(cwd: &std::path::Path) -> std::path::PathBuf {
    super::sidecar::root(cwd).join(".snapshot")
}

fn cache_path(backend: &str, scope: &str) -> Option<std::path::PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    Some(cache_dir(&cwd).join(format!(
        "{backend}-{}.json",
        crate::claims::encode_key(scope)
    )))
}

fn cache_write(backend: &str, scope: &str, doc: &Value) {
    let Some(path) = cache_path(backend, scope) else {
        return;
    };
    // Only the write creates the directory; a cache read never mutates.
    let Some(dir) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let payload = json!({ "taken_at": now_rfc3339(), "snapshot": doc });
    let tmp = path.with_file_name(format!(".tmp-snapshot-{}", std::process::id()));
    if std::fs::write(&tmp, payload.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn cache_read(backend: &str, scope: &str) -> Option<(String, Value)> {
    let path = cache_path(backend, scope)?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    let taken_at = v.get("taken_at").and_then(Value::as_str)?.to_string();
    let doc = v.get("snapshot")?.clone();
    Some((taken_at, doc))
}

/// The backend-neutral joined view: one entry per open candidate joined with
/// its sidecar, tombstones for closed blockers, and the closed window. A
/// `list_open` or sidecar failure fails the whole build (selection fails
/// closed on either today); a closed-window failure only adds an errors line.
pub fn build(t: &dyn Tracker, backend: &str) -> Result<Value, TrackerError> {
    let mut errors: Vec<String> = Vec::new();
    let candidates = t.list_open()?;
    let open_ids: HashSet<&str> = candidates.iter().map(|c| c.node.id.as_str()).collect();
    let mut blocker_ids: BTreeSet<String> = BTreeSet::new();
    for c in &candidates {
        for b in &c.node.blocked_by {
            if !open_ids.contains(b.as_str()) {
                blocker_ids.insert(b.clone());
            }
        }
    }
    // Tombstones for closed dependencies referenced by open items. An
    // unresolvable blocker id is skipped: the consumer's own fail-closed rule
    // (unknown dep == blocked) is the correct outcome there, and this loop
    // must not invent an opinion about a backend read that errored.
    let mut tombstones = Vec::new();
    for bid in &blocker_ids {
        if let Ok(node) = t.read(bid) {
            if node.state == State::Closed {
                tombstones.push(json!(
                    {"id": bid, "state": "closed", "status": "done",
                     "completed_at": CLOSED_STAMP}
                ));
            }
        }
    }
    let mut entries: Vec<Value> = Vec::new();
    for c in &candidates {
        if c.node.id.contains(':') {
            errors.push(format!("skipped id containing ':': {}", c.node.id));
            continue;
        }
        let sc = t.sidecar(&c.node.id)?;
        let mut obj = Map::new();
        obj.insert("id".into(), json!(c.node.id));
        obj.insert(
            "slug".into(),
            json!(c
                .node
                .title
                .as_deref()
                .map(crate::graph_store::derive_base_slug)
                .unwrap_or_default()),
        );
        obj.insert("title".into(), json!(c.node.title));
        obj.insert("state".into(), json!("open"));
        obj.insert(
            "status".into(),
            json!(open_status(sc.get("pr_number"), sc.get("plan_path"))),
        );
        obj.insert("priority".into(), json!(c.priority));
        obj.insert("rank".into(), json!(c.rank));
        obj.insert("created_at".into(), json!(c.created_at));
        obj.insert("parent".into(), json!(c.node.parent));
        obj.insert("blocked_by".into(), json!(c.node.blocked_by));
        obj.insert("details".into(), json!(c.node.details));
        obj.insert("url".into(), json!(c.node.url));
        obj.insert("size".into(), json!(c.node.size));
        merge_sidecar(&mut obj, sc, &mut errors);
        entries.push(Value::Object(obj));
    }
    entries.extend(tombstones);
    if let Some(window) = t.list_closed_since(CLOSED_WINDOW_DAYS) {
        match window {
            Ok(items) => {
                for c in items {
                    if c.node.id.contains(':') {
                        errors.push(format!("skipped id containing ':': {}", c.node.id));
                        continue;
                    }
                    let sc = t.sidecar(&c.node.id)?;
                    let mut obj = Map::new();
                    obj.insert("id".into(), json!(c.node.id));
                    obj.insert(
                        "slug".into(),
                        json!(c
                            .node
                            .title
                            .as_deref()
                            .map(crate::graph_store::derive_base_slug)
                            .unwrap_or_default()),
                    );
                    obj.insert("title".into(), json!(c.node.title));
                    obj.insert("state".into(), json!("closed"));
                    obj.insert("status".into(), json!("done"));
                    obj.insert(
                        "completed_at".into(),
                        json!(c
                            .closed_at
                            .clone()
                            .unwrap_or_else(|| CLOSED_STAMP.to_string())),
                    );
                    obj.insert("priority".into(), json!(c.priority));
                    obj.insert("rank".into(), json!(c.rank));
                    obj.insert("created_at".into(), json!(c.created_at));
                    obj.insert("parent".into(), json!(c.node.parent));
                    obj.insert("blocked_by".into(), json!(c.node.blocked_by));
                    obj.insert("details".into(), json!(c.node.details));
                    obj.insert("url".into(), json!(c.node.url));
                    obj.insert("size".into(), json!(c.node.size));
                    merge_sidecar(&mut obj, sc, &mut errors);
                    entries.push(Value::Object(obj));
                }
            }
            Err(e) => errors.push(format!("closed window failed: {e}")),
        }
    }
    Ok(json!({"backend": backend, "entries": entries, "errors": errors}))
}

/// The port of `_external_open_status`: a PR means in_review, else a plan
/// means ready, else idea. The one read-time status derivation every
/// external-backend open entry derives from.
fn open_status(pr_number: Option<&Value>, plan_path: Option<&Value>) -> &'static str {
    if pr_number.and_then(Value::as_i64).unwrap_or(0) != 0 {
        return "in_review";
    }
    match plan_path.and_then(Value::as_str) {
        Some(p) if !p.is_empty() => "ready",
        _ => "idea",
    }
}

/// Merge the sidecar map into an entry. A key naming a tracker field is the
/// partition breach: it is dropped and named in `errors` instead of
/// overwriting.
fn merge_sidecar(obj: &mut Map<String, Value>, sc: Map<String, Value>, errors: &mut Vec<String>) {
    for (k, v) in sc {
        if k == "id" {
            continue;
        }
        if TRACKER_FIELDS.contains(&k.as_str()) {
            errors.push(format!("sidecar key {k:?} names a tracker field; dropped"));
            continue;
        }
        obj.insert(k, v);
    }
}

/// The tracker-side field names, for the drop-and-name rule above.
const TRACKER_FIELDS: &[&str] = &[
    "id",
    "title",
    "state",
    "parent",
    "blocked_by",
    "details",
    "url",
    "size",
    "priority",
    "rank",
    "created_at",
    "closed_at",
    "slug",
    "status",
];
