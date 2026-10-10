//! The merge-hold writer behind the `authorized-merge` verb's `op` field.
//!
//! A team or worker pipes `{"op": "hold-set"|"hold-release", ...}` straight
//! into `fno-agents authorized-merge` (taught in the lead and blueprint
//! skills). The block it writes is the same
//! `dispatch_hold` frontmatter every merge path already reads; the write is
//! proven by the same reader ready selection uses, and a failed readback
//! restores the original bytes. A release of a hold the user set, or of one
//! naming a question, must cite a live superuser-lane ruling at the held
//! node or at the question it names. Rides an existing verb rather than a
//! new top-level root, and keeps the writer out of the Python tree the file
//! budget caps.

use crate::backlog_ready::{
    dispatch_hold, dispatch_hold_verdict, read_frontmatter, resolve_plan_probe, HoldState,
};
use crate::decision_index;
use crate::graph_get::{default_graph_path, find_entry};
use crate::graph_store;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Run one hold op from an `authorized-merge` payload. Always answers with a
/// JSON receipt (`outcome`, `exit_code` inside); the verb's exit status
/// answers only whether the op RAN.
pub fn run(op: &str, payload: &Value) -> String {
    let node = payload.get("node").and_then(Value::as_str).unwrap_or("");
    if node.is_empty() {
        return receipt("refused", 2, "payload needs a node id or slug").to_string();
    }
    // The verdict answers from payload rows when they ride the ask (the
    // tests' in-memory graphs): no graph read at all on that path.
    if op.strip_prefix("hold-").unwrap_or(op) == "verdict" {
        if let Some(rows) = payload.get("entries").and_then(Value::as_array) {
            return verdict_receipt_rows(node, rows);
        }
    }
    let graph = payload
        .get("graph")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(default_graph_path);
    let entries = match crate::graph_store::read_rows_where(
        &graph,
        &crate::backlog::RowQuery {
            fields: Some(
                crate::graph_store::SLIM_FIELDS
                    .iter()
                    .copied()
                    .chain([
                        "contained_in",
                        "dispatch_hold",
                        "superseded_by",
                        "deferred_at",
                    ])
                    .map(str::to_string)
                    .collect(),
            ),
            with_blockers: true,
            ..Default::default()
        },
    ) {
        Ok(e) => e,
        Err(e) => {
            return receipt("refused", 5, format!("graph read failed: {e}")).to_string();
        }
    };
    let entry = match find_entry(&entries, node) {
        Some(e) => e.clone(),
        None => {
            return receipt("refused", 2, format!("no node resolves to '{node}'")).to_string();
        }
    };
    let by_id: BTreeMap<String, Value> = entries
        .iter()
        .filter_map(|e| graph_store::entry_id(e).map(|id| (id.to_string(), e.clone())))
        .collect();
    let node_id = entry
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(node)
        .to_string();
    match op.strip_prefix("hold-").unwrap_or(op) {
        "set" => set_hold(&entry, &node_id, payload, &graph),
        "release" => {
            let decisions = crate::decision_index::default_state_path("decisions.jsonl");
            release_hold(&entry, &node_id, payload, &entries, &graph, &decisions)
        }
        // The one hold verdict the merge and dispatch gates ask for: the
        // reader walks the bounded ancestry and answers with the first
        // hold, fields flattened for the receipt.
        "verdict" => {
            let Some(v) = crate::backlog_ready::hold_verdict_receipt(&entry, &by_id) else {
                return receipt("absent", 0, "").to_string();
            };
            verdict_receipt(&v)
        }
        other => receipt("refused", 2, format!("unknown hold op: {other}")).to_string(),
    }
}

/// The verdict receipt body shared by the disk and payload-row paths.
fn verdict_receipt(v: &crate::backlog_ready::HoldVerdictReceipt) -> String {
    let mut out = receipt(if v.held { "held" } else { "invalid" }, 0, "");
    if let Some(obj) = out.as_object_mut() {
        obj.insert("owner".into(), Value::String(v.owner.clone()));
        obj.insert("guard_reason".into(), Value::String(v.guard_reason.clone()));
        obj.insert("reason".into(), Value::String(v.reason.clone()));
        obj.insert("release_when".into(), Value::String(v.release_when.clone()));
        obj.insert("review_on".into(), Value::String(v.review_on.clone()));
        obj.insert("set_by".into(), Value::String(v.set_by.clone()));
        obj.insert("detail".into(), Value::String(v.detail.clone()));
    }
    out.to_string()
}

/// The verdict answered from the ask's own rows: the tests' in-memory
/// graphs, no graph read at all.
fn verdict_receipt_rows(node: &str, rows: &[Value]) -> String {
    let by_id: BTreeMap<String, Value> = rows
        .iter()
        .filter_map(|e| graph_store::entry_id(e).map(|id| (id.to_string(), e.clone())))
        .collect();
    let entry = by_id.get(node).cloned().unwrap_or(Value::Null);
    match crate::backlog_ready::hold_verdict_receipt(&entry, &by_id) {
        Some(v) => verdict_receipt(&v),
        None => receipt("absent", 0, "").to_string(),
    }
}

/// One hold receipt: `outcome` + `exit_code` (0 done, 2 bad input, 3 state
/// refusal, 1 write/readback failure, 5 graph read failed) + `detail` on a
/// refusal.
fn receipt(outcome: &str, code: i32, detail: impl Into<String>) -> Value {
    json!({"outcome": outcome, "exit_code": code, "detail": detail.into()})
}

/// How long a hold op waits for another hold op on the same plan.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Exclusive flock serializing concurrent hold ops on one plan: the
/// read-modify-write is not atomic as a whole, and a team setting while a
/// worker releases would silently drop one ruling.
struct PlanLock {
    /// Held for the lock's lifetime; the flock dies with this handle.
    _file: File,
}

impl PlanLock {
    fn acquire(plan: &Path) -> Result<PlanLock, Value> {
        let lock_path = PathBuf::from(format!("{}.lock", plan.display()));
        if let Some(parent) = lock_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|e| receipt("error", 2, format!("hold lock open failed: {e}")))?;
        let deadline = std::time::Instant::now() + LOCK_TIMEOUT;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(PlanLock { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(receipt(
                            "error",
                            2,
                            format!("hold lock timeout after 10s at {}", lock_path.display()),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(receipt("error", 2, format!("hold lock failed: {e}"))),
            }
        }
    }
}

fn resolve_plan(entry: &Value, node_id: &str) -> Result<std::path::PathBuf, Value> {
    match resolve_plan_probe(entry) {
        Some(probe) if probe.exists() => Ok(probe),
        probe => Err(receipt(
            "refused",
            2,
            format!(
                "node {node_id} has no usable plan file{}; a merge hold lives in plan frontmatter, so the node needs a blueprint first",
                probe
                    .map(|p| format!(" ({})", p.display()))
                    .unwrap_or_default()
            ),
        )),
    }
}

/// Atomic write: temp file in the plan's own directory, then rename.
fn atomic_write(probe: &Path, text: &str) -> Result<(), String> {
    let dir = probe.parent().unwrap_or_else(|| Path::new("."));
    let name = probe
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "plan".to_string());
    let tmp = dir.join(format!(".{name}.hold.tmp"));
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, probe)
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            e
        })
        .map_err(|e| e.to_string())
}

/// Write, then prove with the reader the merge gate uses; a miss restores.
fn write_proven(
    probe: &Path,
    entry: &Value,
    new_text: &str,
    original: &str,
    want: HoldState,
) -> Option<Value> {
    if let Err(e) = atomic_write(probe, new_text) {
        return Some(receipt("error", 2, format!("plan write failed: {e}")));
    }
    let state = dispatch_hold(entry);
    if std::mem::discriminant(&state) == std::mem::discriminant(&want) {
        return None;
    }
    let restored = atomic_write(probe, original).is_ok();
    let word = |s: &HoldState| match s {
        HoldState::Absent => "ABSENT",
        HoldState::Held => "HELD",
        HoldState::Invalid => "INVALID",
    };
    Some(receipt(
        "error",
        1,
        format!(
            "readback answered {} where {} was required; {}",
            word(&state),
            word(&want),
            if restored {
                "original bytes restored"
            } else {
                "RESTORE FAILED - inspect the plan by hand"
            }
        ),
    ))
}

fn insert_before_closing_fence(text: &str, block: &str) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.first().copied() != Some("---") {
        return None;
    }
    let close = (1..lines.len()).find(|&i| lines[i] == "---")?;
    let mut out = String::with_capacity(text.len() + block.len());
    for line in &lines[..close] {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(block.trim_end_matches('\n'));
    out.push('\n');
    out.push_str(&lines[close..].join("\n"));
    Some(out)
}

/// Remove the `dispatch_hold:` line and its indented/blank run.
fn remove_hold_block(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.first().copied() != Some("---") {
        return None;
    }
    let close = (1..lines.len()).find(|&i| lines[i] == "---")?;
    let start = (1..close).find(|&i| lines[i].starts_with("dispatch_hold:"))?;
    let mut end = start + 1;
    while end < close && (lines[end].is_empty() || lines[end].starts_with([' ', '\t'])) {
        end += 1;
    }
    let mut out = String::with_capacity(text.len());
    for (i, line) in lines.iter().enumerate() {
        if i < start || i >= end {
            out.push_str(line);
            if i + 1 < lines.len() {
                out.push('\n');
            }
        }
    }
    Some(out)
}

fn existing_hold(probe: &Path) -> (String, String) {
    let (r, w, _) = existing_hold_fields(probe);
    (r, w)
}

/// The plan hold's string fields by name ("" when a field is missing). The
/// release guard reads `set_by` and the free text its ruling must govern.
fn existing_hold_fields(probe: &Path) -> (String, String, String) {
    let Some(fm) = read_frontmatter(probe) else {
        return (String::new(), String::new(), String::new());
    };
    let Some(block) = fm.get("dispatch_hold").and_then(Value::as_object) else {
        return (String::new(), String::new(), String::new());
    };
    let field = |k: &str| {
        block
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    (field("reason"), field("release_when"), field("set_by"))
}

fn pr_number(entry: &Value) -> Option<u64> {
    match entry.get("pr_number") {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

/// Best-effort `gh pr merge --disable-auto`: a queue armed before the hold
/// would otherwise merge server-side with no re-check.
pub(crate) fn disarm_automerge(pr: u64) -> String {
    match std::process::Command::new("gh")
        .args(["pr", "merge", &pr.to_string(), "--disable-auto"])
        .output()
    {
        Ok(o) if o.status.success() => "issued".to_string(),
        Ok(o) => format!("failed: gh exited {}", o.status.code().unwrap_or(-1)),
        Err(e) => format!("failed: {e}"),
    }
}

fn set_hold(entry: &Value, node_id: &str, payload: &Value, graph: &Path) -> String {
    let reason = payload_str(payload, "reason").unwrap_or("");
    let release_when = payload_str(payload, "release_when").unwrap_or("");
    let set_by = payload_str(payload, "set_by").unwrap_or("");
    for (name, value) in [
        ("reason", reason),
        ("release-when", release_when),
        ("set-by", set_by),
    ] {
        if value.trim().is_empty() {
            return receipt("refused", 2, format!("set needs a non-blank --{name}")).to_string();
        }
    }
    let review_on = match payload_str(payload, "review_on") {
        Some(s) => s.to_string(),
        None => (chrono::Utc::now().date_naive() + chrono::Duration::days(7))
            .format("%Y-%m-%d")
            .to_string(),
    };
    if chrono::NaiveDate::parse_from_str(review_on.trim(), "%Y-%m-%d").is_err() {
        return receipt(
            "refused",
            2,
            format!("--review-on must parse as YYYY-MM-DD, got: {review_on}"),
        )
        .to_string();
    }
    let probe = match resolve_plan(entry, node_id) {
        Ok(p) => p,
        // A hold no longer needs a plan file. The node row carries
        // the same block, written through the store's locked mutation.
        Err(_) => {
            return set_node_hold(
                entry,
                node_id,
                graph,
                &reason,
                &release_when,
                &set_by,
                &review_on,
            )
        }
    };
    let _lock = match PlanLock::acquire(&probe) {
        Ok(l) => l,
        Err(err) => return err.to_string(),
    };
    if !matches!(dispatch_hold(entry), HoldState::Absent) {
        let (r, w) = existing_hold(&probe);
        return receipt(
            "refused",
            3,
            format!(
                "node {node_id} is already held: reason={r} release_when={w}; lift it with `fno do pr hold release {node_id} --evidence <proof>`"
            ),
        )
        .to_string();
    }
    let mut hold = Map::new();
    hold.insert("reason".into(), Value::String(reason.to_string()));
    hold.insert(
        "release_when".into(),
        Value::String(release_when.to_string()),
    );
    hold.insert("review_on".into(), Value::String(review_on.clone()));
    hold.insert("set_by".into(), Value::String(set_by.to_string()));
    let mut top = Map::new();
    top.insert("dispatch_hold".into(), Value::Object(hold.clone()));
    let block = match serde_yaml_ng::to_string(&Value::Object(top)) {
        Ok(b) => b,
        Err(e) => {
            return receipt("error", 2, format!("block serialization failed: {e}")).to_string()
        }
    };
    let original = match std::fs::read_to_string(&probe) {
        Ok(t) => t,
        Err(e) => return receipt("error", 2, format!("plan read failed: {e}")).to_string(),
    };
    let Some(new_text) = insert_before_closing_fence(&original, &block) else {
        return receipt(
            "error",
            1,
            format!(
                "plan {} has no closing --- fence; refusing to edit",
                probe.display()
            ),
        )
        .to_string();
    };
    if let Some(err) = write_proven(&probe, entry, &new_text, original.as_str(), HoldState::Held) {
        return err.to_string();
    }
    let pr = pr_number(entry);
    let disarm = pr
        .map(disarm_automerge)
        .unwrap_or_else(|| "skipped".to_string());
    let mut out = receipt("held", 0, "");
    if let Some(obj) = out.as_object_mut() {
        obj.insert("node".into(), Value::String(node_id.to_string()));
        obj.insert("action".into(), Value::String("set".to_string()));
        obj.insert("plan".into(), Value::String(probe.display().to_string()));
        obj.insert("hold".into(), Value::Object(hold));
        obj.insert("pr".into(), pr.map(Value::from).unwrap_or(Value::Null));
        obj.insert("disarm".into(), Value::String(disarm.clone()));
    }
    out.to_string()
}

/// The plan-less hold home: the node row's own `dispatch_hold`
/// field, written through the store's locked read-modify-write and proven by
/// the same reader the merge gate uses. Restores (clears the field) when the
/// readback disagrees, the way `write_proven` restores the plan bytes.
fn set_node_hold(
    entry: &Value,
    node_id: &str,
    graph: &Path,
    reason: &str,
    release_when: &str,
    set_by: &str,
    review_on: &str,
) -> String {
    if !matches!(dispatch_hold(entry), HoldState::Absent) {
        let obj = entry.get("dispatch_hold").and_then(Value::as_object);
        let say = |k: &str| {
            obj.and_then(|o| o.get(k))
                .and_then(Value::as_str)
                .unwrap_or("")
        };
        return receipt(
            "refused",
            3,
            format!(
                "node {node_id} is already held: reason={} release_when={}; \
                 lift it with `fno do pr hold release {node_id} --evidence <proof>`",
                say("reason"),
                say("release_when")
            ),
        )
        .to_string();
    }
    let mut hold = Map::new();
    hold.insert("reason".into(), Value::String(reason.to_string()));
    hold.insert(
        "release_when".into(),
        Value::String(release_when.to_string()),
    );
    hold.insert("review_on".into(), Value::String(review_on.to_string()));
    hold.insert("set_by".into(), Value::String(set_by.to_string()));
    let rows = match mutate_node_hold(graph, node_id, Some(Value::Object(hold.clone()))) {
        Ok(rows) => rows,
        Err(err) => return err.to_string(),
    };
    let fresh = find_entry(&rows, node_id)
        .cloned()
        .unwrap_or_else(|| entry.clone());
    if !matches!(dispatch_hold(&fresh), HoldState::Held) {
        let _ = mutate_node_hold(graph, node_id, None);
        return receipt(
            "error",
            1,
            format!("node {node_id} hold readback missed Held; the field was cleared"),
        )
        .to_string();
    }
    let pr = pr_number(entry);
    let disarm = pr
        .map(disarm_automerge)
        .unwrap_or_else(|| "skipped".to_string());
    let mut out = receipt("held", 0, "");
    if let Some(obj) = out.as_object_mut() {
        obj.insert("node".into(), Value::String(node_id.to_string()));
        obj.insert("action".into(), Value::String("set".to_string()));
        obj.insert("plan".into(), Value::Null);
        obj.insert("hold".into(), Value::Object(hold));
        obj.insert("pr".into(), pr.map(Value::from).unwrap_or(Value::Null));
        obj.insert("disarm".into(), Value::String(disarm.clone()));
    }
    out.to_string()
}

/// Locked read-modify-write of the node row's `dispatch_hold` field
/// (`Some` sets it, `None` clears it), with the patch door's contention
/// retry. Returns the committed rows for the readback.
fn mutate_node_hold(graph: &Path, node_id: &str, hold: Option<Value>) -> Result<Vec<Value>, Value> {
    const ATTEMPTS: usize = 3;
    for attempt in 0..ATTEMPTS {
        let version = graph_store::base_version(graph)
            .map_err(|e| receipt("error", 5, format!("graph read failed: {e}")))?;
        let mut rows = graph_store::read_rows(graph)
            .map_err(|e| receipt("error", 5, format!("graph read failed: {e}")))?;
        let idx = rows
            .iter()
            .position(|e| graph_store::entry_id(e) == Some(node_id))
            .ok_or_else(|| receipt("refused", 2, format!("no node resolves to '{node_id}'")))?;
        {
            let obj = rows[idx].as_object_mut().unwrap();
            match &hold {
                Some(h) => {
                    obj.insert("dispatch_hold".to_string(), h.clone());
                }
                None => {
                    obj.shift_remove("dispatch_hold");
                }
            }
        }
        let rungs: BTreeMap<String, String> = rows
            .iter()
            .filter_map(|e| {
                graph_store::entry_id(e).map(|id| {
                    (
                        id.to_string(),
                        crate::backlog_ready::plan_rung(e).to_string(),
                    )
                })
            })
            .collect();
        match graph_store::locked_mutate(
            graph,
            graph_store::MutateInput {
                entries: rows,
                canonical_path: None,
                base_version: version,
                plan_rungs: Some(rungs),
            },
            graph_store::DEFAULT_LOCK_TIMEOUT,
        ) {
            Ok(outcome) => return Ok(outcome.entries),
            Err(graph_store::StoreError::Conflict | graph_store::StoreError::LockTimeout(..))
                if attempt + 1 < ATTEMPTS =>
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(graph_store::StoreError::Conflict) => {
                return Err(receipt(
                    "error",
                    1,
                    format!("graph changed under the hold write after {ATTEMPTS} attempts"),
                ));
            }
            Err(graph_store::StoreError::LockTimeout(..)) => {
                return Err(receipt(
                    "error",
                    1,
                    format!("the graph lock stayed busy across {ATTEMPTS} attempts"),
                ));
            }
            Err(e) => return Err(receipt("error", 5, format!("graph write failed: {e}"))),
        }
    }
    unreachable!("every loop arm returns")
}

fn payload_str<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
}

fn release_hold(
    entry: &Value,
    node_id: &str,
    payload: &Value,
    entries: &[Value],
    graph: &Path,
    decisions_jsonl: &Path,
) -> String {
    let evidence = payload_str(payload, "evidence").unwrap_or("");
    if evidence.trim().is_empty() {
        return receipt("refused", 2, "release needs a non-blank --evidence").to_string();
    }
    let probe = match resolve_plan(entry, node_id) {
        Ok(p) => p,
        Err(_) => {
            return release_node_hold(entry, node_id, graph, evidence, decisions_jsonl);
        }
    };
    let _lock = match PlanLock::acquire(&probe) {
        Ok(l) => l,
        Err(err) => return err.to_string(),
    };
    if matches!(dispatch_hold(entry), HoldState::Absent) {
        return receipt(
            "refused",
            3,
            format!("node {node_id} carries no merge hold; nothing to release"),
        )
        .to_string();
    }
    let (hold_reason, hold_when, hold_set_by) = existing_hold_fields(&probe);
    if let Some(refusal) = release_evidence_refusal(
        node_id,
        &hold_set_by,
        &format!("{hold_reason}\n{hold_when}"),
        evidence,
        graph,
        decisions_jsonl,
    ) {
        return refusal.to_string();
    }
    let original = match std::fs::read_to_string(&probe) {
        Ok(t) => t,
        Err(e) => return receipt("error", 2, format!("plan read failed: {e}")).to_string(),
    };
    let Some(new_text) = remove_hold_block(&original) else {
        return receipt(
            "error",
            3,
            format!(
                "plan {} carries no dispatch_hold block to remove",
                probe.display()
            ),
        )
        .to_string();
    };
    if let Some(err) = write_proven(&probe, entry, &new_text, &original, HoldState::Absent) {
        return err.to_string();
    }
    let by_id: BTreeMap<String, Value> = entries
        .iter()
        .filter_map(|e| {
            e.get("id")
                .and_then(Value::as_str)
                .map(|id| (id.to_string(), e.clone()))
        })
        .collect();
    let still_held_by = dispatch_hold_verdict(entry, &by_id).map(|v| v.guard_reason);
    let mut out = receipt("released", 0, "");
    if let Some(obj) = out.as_object_mut() {
        obj.insert("node".into(), Value::String(node_id.to_string()));
        obj.insert("action".into(), Value::String("release".into()));
        obj.insert("plan".into(), Value::String(probe.display().to_string()));
        obj.insert(
            "hold".into(),
            json!({"evidence": evidence, "still_held_by": still_held_by}),
        );
        obj.insert("disarm".into(), Value::String("skipped".into()));
    }
    out.to_string()
}

/// Release the node row's own hold field (the plan-less arm). The
/// verdict reads the FRESH row - the stale `entry` still carries the field
/// this op just cleared.
fn release_node_hold(
    entry: &Value,
    node_id: &str,
    graph: &Path,
    evidence: &str,
    decisions_jsonl: &Path,
) -> String {
    match entry.get("dispatch_hold") {
        None | Some(Value::Null) => {
            return receipt(
                "refused",
                3,
                format!("node {node_id} carries no merge hold; nothing to release"),
            )
            .to_string();
        }
        _ => {}
    }
    let hold = entry.get("dispatch_hold");
    let field = |k: &str| {
        hold.and_then(|h| h.get(k))
            .and_then(Value::as_str)
            .unwrap_or("")
    };
    if let Some(refusal) = release_evidence_refusal(
        node_id,
        field("set_by"),
        &format!("{}\n{}", field("reason"), field("release_when")),
        evidence,
        graph,
        decisions_jsonl,
    ) {
        return refusal.to_string();
    }
    let fresh_rows = match mutate_node_hold(graph, node_id, None) {
        Ok(rows) => rows,
        Err(err) => return err.to_string(),
    };
    let fresh = find_entry(&fresh_rows, node_id)
        .cloned()
        .unwrap_or_else(|| entry.clone());
    let by_id: BTreeMap<String, Value> = fresh_rows
        .iter()
        .filter_map(|e| {
            e.get("id")
                .and_then(Value::as_str)
                .map(|id| (id.to_string(), e.clone()))
        })
        .collect();
    let still_held_by = dispatch_hold_verdict(&fresh, &by_id).map(|v| v.guard_reason);
    let mut out = receipt("released", 0, "");
    if let Some(obj) = out.as_object_mut() {
        obj.insert("node".into(), Value::String(node_id.to_string()));
        obj.insert("action".into(), Value::String("release".into()));
        obj.insert("plan".into(), Value::Null);
        obj.insert(
            "hold".into(),
            json!({"evidence": evidence, "still_held_by": still_held_by}),
        );
        obj.insert("disarm".into(), Value::String("skipped".into()));
    }
    out.to_string()
}

/// The question ids a hold's free text names (`q-xc129`), lowercased, in
/// order. A hold names a question by writing its id in `reason` or
/// `release_when`; the ruling that lifts it may sit at
/// `question:<qid>` instead of at the node.
fn named_question_ids(hold_text: &str) -> Vec<String> {
    static QID: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = QID.get_or_init(|| regex::Regex::new(r"(?i)\bq-[0-9a-z]{1,}\b").expect("static"));
    let mut ids: Vec<String> = Vec::new();
    for cap in re.captures_iter(hold_text) {
        let id = cap[0].to_lowercase();
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

/// The release guard: a hold the user set, or one naming a question, lifts
/// only on a live superuser-lane ruling (operator or chat_attested, the
/// `decision_index::is_law` test) at the held node or at the question it
/// names. Measured 2026-09-30: any non-blank text released a user's hold,
/// and a ruling recorded at a different subject was read as lifting it
/// (2026-10-09 live case). A crown ruling lifts only a hold it governs; a
/// team hold naming no question releases as before. The decisions path is a
/// parameter, never a payload key: the caller must not name the store that
/// proves the release. `Some` carries the refusal receipt.
fn release_evidence_refusal(
    node_id: &str,
    set_by: &str,
    hold_text: &str,
    evidence: &str,
    graph: &Path,
    decisions_jsonl: &Path,
) -> Option<Value> {
    let qids = named_question_ids(hold_text);
    if !set_by.eq_ignore_ascii_case("user") && qids.is_empty() {
        return None;
    }
    let where_a_ruling_lifts = if qids.is_empty() {
        format!("node {node_id}")
    } else {
        let named: Vec<String> = qids.iter().map(|q| format!("question:{q}")).collect();
        format!("node {node_id} or {}", named.join(" or "))
    };
    let index = match decision_index::read_store_live(graph, decisions_jsonl) {
        Ok(i) => i,
        Err(e) => {
            return Some(receipt(
                "refused",
                5,
                format!(
                    "the decision store is unreadable ({e}); refusing to assume \
                     the release is lawful; a live ruling at {where_a_ruling_lifts} lifts this hold"
                ),
            ))
        }
    };
    let wanted = evidence.trim().to_lowercase();
    let row = index.rows.iter().find(|r| {
        r.get("decision_id")
            .and_then(Value::as_str)
            .is_some_and(|id| id.to_lowercase() == wanted)
    });
    let Some(row) = row else {
        return Some(receipt(
            "refused",
            3,
            format!(
                "no live decision '{evidence}': this hold was set by {set_by}, so the \
                 release evidence must be a live superuser-lane ruling at {where_a_ruling_lifts}"
            ),
        ));
    };
    if !decision_index::is_law(row) {
        let authority = row
            .get("authority_source")
            .and_then(Value::as_str)
            .unwrap_or("");
        return Some(receipt(
            "refused",
            3,
            format!(
                "decision {evidence} is not superuser lane (authority_source={authority}): \
                 only an operator or chat_attested ruling at {where_a_ruling_lifts} lifts this hold"
            ),
        ));
    }
    let subject = row.get("subject").and_then(Value::as_str).unwrap_or("");
    let at_named_question = qids
        .iter()
        .any(|q| subject.eq_ignore_ascii_case(&format!("question:{q}")));
    if !subject.eq_ignore_ascii_case(node_id) && !at_named_question {
        return Some(receipt(
            "refused",
            3,
            format!(
                "decision {evidence} sits at subject '{subject}', which governs nothing \
                 here: a crown ruling lifts only a hold at {where_a_ruling_lifts}"
            ),
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PLAN_BODY: &str = "---\nclaims: t-0001\nstatus: ready\nkind: quick-plan\npriority: p1\n---\n\n# A plan\n\nBody.\n";

    struct Fixture {
        _dir: tempfile::TempDir,
        graph: std::path::PathBuf,
        plan: std::path::PathBuf,
    }

    fn fixture(extra: Value) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("plan.md");
        std::fs::write(&plan, PLAN_BODY).unwrap();
        let mut entry = json!({
            "id": "t-0001",
            "slug": "a-plan",
            "plan_path": plan.display().to_string(),
            "cwd": dir.path().display().to_string(),
        });
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                entry[k.as_str()] = v.clone();
            }
        }
        let graph = dir.path().join("graph.json");
        crate::graph_store::seed_rows(&graph, &[entry]).unwrap();
        Fixture {
            _dir: dir,
            graph,
            plan,
        }
    }

    fn set_payload(graph_path: String) -> Value {
        json!({
            "op": "hold-set",
            "node": "t-0001",
            "reason": "condition R",
            "release_when": "when W",
            "set_by": "team",
            "graph": graph_path,
        })
    }

    fn release_payload(graph_path: String, evidence: &str) -> Value {
        json!({
            "op": "hold-release",
            "node": "t-0001",
            "evidence": evidence,
            "graph": graph_path,
        })
    }

    fn hold_entry(f: &Fixture) -> Value {
        crate::graph_store::read_rows(&f.graph).unwrap()[0].clone()
    }

    #[test]
    fn set_holds_and_the_reader_answers_held() {
        let fx = fixture(json!({}));
        let out = run("hold-set", &set_payload(fx.graph.display().to_string()));
        eprintln!("DEBUG-RECEIPT: {out}");
        let receipt: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(receipt["outcome"], "held");
        assert_eq!(receipt["exit_code"], 0);
        assert_eq!(receipt["hold"]["reason"], "condition R");
        assert!(matches!(dispatch_hold(&hold_entry(&fx)), HoldState::Held));
        assert_eq!(
            remove_hold_block(&std::fs::read_to_string(&fx.plan).unwrap()).unwrap(),
            PLAN_BODY
        );
    }

    #[test]
    fn set_refusals_write_nothing() {
        let fx = fixture(json!({}));
        let g = fx.graph.display().to_string();
        let payloads = [
            json!({"op": "hold-set", "node": "t-nope", "reason": "r", "release_when": "w", "set_by": "s", "graph": g}),
            json!({"op": "hold-set", "node": "t-0001", "reason": "  ", "release_when": "w", "set_by": "s", "graph": g}),
            json!({"op": "hold-set", "node": "t-0001", "reason": "r", "release_when": "w", "set_by": "s", "review_on": "2026-13-99", "graph": g}),
            json!({"op": "hold-set", "node": "t-0001", "graph": g}),
        ];
        for payload in payloads {
            let out = run("hold-set", &payload);
            let r: Value = serde_json::from_str(&out).unwrap();
            assert_eq!(r["exit_code"], 2, "{out}");
        }
        let out = run("hold-set", &json!({"op": "hold-set"}));
        assert!(out.contains("node"), "{out}");
        assert_eq!(std::fs::read_to_string(&fx.plan).unwrap(), PLAN_BODY);
    }

    #[test]
    fn set_refuses_an_already_held_plan() {
        let fx = fixture(json!({}));
        run("hold-set", &set_payload(fx.graph.display().to_string()));
        let out = run("hold-set", &set_payload(fx.graph.display().to_string()));
        let r: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(r["exit_code"], 3);
        assert!(out.contains("condition R"), "{out}");
        assert!(out.contains("hold release"), "{out}");
        let text = std::fs::read_to_string(&fx.plan).unwrap();
        assert_eq!(text.matches("dispatch_hold:").count(), 1);
    }

    /// A corrupt store carries its own code (5) and names the read failure;
    /// an absent id on a well-formed store stays exit 2.
    #[test]
    fn corrupt_graph_refusal_is_its_own_code_not_absence() {
        let dir = tempfile::tempdir().unwrap();
        let corrupt = dir.path().join("graph.json");
        std::fs::write(&corrupt, "{").unwrap();
        let out = run(
            "hold-set",
            &json!({"op": "hold-set", "node": "t-0001", "reason": "r",
                    "release_when": "w", "set_by": "s",
                    "graph": corrupt.display().to_string()}),
        );
        let r: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(r["exit_code"], 5, "{out}");
        assert!(out.contains("graph read failed"), "{out}");
        assert!(!out.contains("no node resolves"), "{out}");

        let fx = fixture(json!({}));
        let out = run(
            "hold-set",
            &json!({"op": "hold-set", "node": "t-nope", "reason": "r",
                    "release_when": "w", "set_by": "s",
                    "graph": fx.graph.display().to_string()}),
        );
        let r: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(r["exit_code"], 2, "{out}");
        assert!(out.contains("no node resolves to 't-nope'"), "{out}");
    }

    #[test]
    fn release_lifts_and_restores_the_original_bytes() {
        let fx = fixture(json!({}));
        run("hold-set", &set_payload(fx.graph.display().to_string()));
        let out = run(
            "hold-release",
            &release_payload(fx.graph.display().to_string(), "condition held"),
        );
        let r: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(r["outcome"], "released");
        assert_eq!(r["hold"]["evidence"], "condition held");
        assert!(r["hold"]["still_held_by"].is_null());
        assert!(matches!(dispatch_hold(&hold_entry(&fx)), HoldState::Absent));
        assert_eq!(std::fs::read_to_string(&fx.plan).unwrap(), PLAN_BODY);
    }

    #[test]
    fn release_refusals() {
        let fx = fixture(json!({}));
        let out = run(
            "hold-release",
            &release_payload(fx.graph.display().to_string(), "early"),
        );
        let r: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(r["exit_code"], 3);
        assert_eq!(std::fs::read_to_string(&fx.plan).unwrap(), PLAN_BODY);
        run("hold-set", &set_payload(fx.graph.display().to_string()));
        let mut no_evidence = release_payload(fx.graph.display().to_string(), "x");
        no_evidence["evidence"] = json!("");
        let out = run("hold-release", &no_evidence);
        let r: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(r["exit_code"], 2);
    }

    #[test]
    fn a_failed_readback_restores_the_original_bytes() {
        let fx = fixture(json!({}));
        let original = std::fs::read_to_string(&fx.plan).unwrap();
        let entry = hold_entry(&fx);
        let err = write_proven(
            &fx.plan,
            &entry,
            &format!("{original}dispatch_hold: 42\n"),
            &original,
            HoldState::Held,
        );
        let err = err.unwrap();
        assert_eq!(err["exit_code"], 1);
        let detail = err["detail"].as_str().unwrap();
        assert!(detail.contains("readback answered"), "{detail}");
        assert!(detail.contains("restored"), "{detail}");
        assert_eq!(std::fs::read_to_string(&fx.plan).unwrap(), original);
    }

    #[test]
    fn release_names_an_ancestor_that_still_holds() {
        let dir = tempfile::tempdir().unwrap();
        let parent_plan = dir.path().join("parent.md");
        std::fs::write(&parent_plan, PLAN_BODY).unwrap();
        let child_plan = dir.path().join("child.md");
        std::fs::write(&child_plan, PLAN_BODY).unwrap();
        let graph = dir.path().join("graph.json");
        let rows = vec![
            json!({"id": "t-parent", "slug": "parent", "plan_path": parent_plan.display().to_string(), "cwd": dir.path().display().to_string()}),
            json!({"id": "t-0001", "slug": "a-plan", "parent": "t-parent", "plan_path": child_plan.display().to_string(), "cwd": dir.path().display().to_string()}),
        ];
        crate::graph_store::seed_rows(&graph, &rows).unwrap();
        let fx = Fixture {
            _dir: dir,
            graph,
            plan: child_plan,
        };
        let mut parent_set = set_payload(fx.graph.display().to_string());
        parent_set["node"] = json!("t-parent");
        run("hold-set", &parent_set);
        run("hold-set", &set_payload(fx.graph.display().to_string()));
        let out = run(
            "hold-release",
            &release_payload(fx.graph.display().to_string(), "condition held"),
        );
        let r: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(r["hold"]["still_held_by"], "dispatch-hold:t-parent");
    }

    // --- the release guard: user-set and question-naming holds ----

    fn seed_decision(path: &std::path::Path, id: &str, subject: &str, authority: &str) {
        let line = json!({
            "type": "operator_decision",
            "ts": "2026-10-01T00:00:00Z",
            "data": {
                "decision_id": id,
                "subject": subject,
                "decision": "Ruling.",
                "text": "Ruling.",
                "authority_source": authority,
            },
        });
        let mut text = std::fs::read_to_string(path).unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&line.to_string());
        text.push('\n');
        std::fs::write(path, text).unwrap();
    }

    fn release_with_decisions(
        graph: &std::path::Path,
        evidence: &str,
        decisions: &std::path::Path,
    ) -> Value {
        let rows = crate::graph_store::read_rows(graph).unwrap();
        let out = release_hold(
            &rows[0],
            "t-0001",
            &release_payload(graph.display().to_string(), evidence),
            &rows,
            graph,
            decisions,
        );
        serde_json::from_str(&out).unwrap()
    }

    #[test]
    fn a_user_set_hold_lifts_only_on_a_live_ruling_at_the_node() {
        let fx = fixture(json!({}));
        let decisions = fx._dir.path().join("decisions.jsonl");
        let mut set = set_payload(fx.graph.display().to_string());
        set["set_by"] = json!("user");
        run("hold-set", &set);

        // Free text refuses: the old non-blank read is gone, the block stays.
        let r = release_with_decisions(&fx.graph, "the freeze is lifted", &decisions);
        assert_eq!(r["outcome"], "refused", "{r}");
        assert_eq!(r["exit_code"], 3);
        assert!(r["detail"].as_str().unwrap().contains("no live decision"));
        assert!(matches!(dispatch_hold(&hold_entry(&fx)), HoldState::Held));

        // A live operator row at the held node lifts it.
        seed_decision(&decisions, "d-02a1a", "t-0001", "operator");
        let r = release_with_decisions(&fx.graph, "d-02a1a", &decisions);
        assert_eq!(r["outcome"], "released", "{r}");
        assert!(matches!(dispatch_hold(&hold_entry(&fx)), HoldState::Absent));

        // The plan-less arm reads the same fields off the node row, and an
        // unreadable store refuses instead of reading as empty.
        let (_dir, graph) = fixture_plan_less(json!({}));
        let decisions = _dir.path().join("decisions.jsonl");
        let mut set = set_payload(graph.display().to_string());
        set["set_by"] = json!("user");
        run("hold-set", &set);
        let r = release_with_decisions(&graph, "any words", &decisions);
        assert_eq!(r["outcome"], "refused", "{r}");
        seed_decision(&decisions, "d-02a1b", "t-0001", "operator");
        let r = release_with_decisions(&graph, "d-02a1b", &decisions);
        assert_eq!(r["outcome"], "released", "{r}");
    }

    #[test]
    fn the_ruling_must_govern_the_hold_foreign_subjects_and_lanes_refuse() {
        let fx = fixture(json!({}));
        let decisions = fx._dir.path().join("decisions.jsonl");
        let mut set = set_payload(fx.graph.display().to_string());
        set["set_by"] = json!("user");
        run("hold-set", &set);

        // A live operator row at a DIFFERENT subject governs nothing here;
        // the 2026-10-09 live case read one as a full release.
        seed_decision(&decisions, "d-foreign", "t-other", "operator");
        let r = release_with_decisions(&fx.graph, "d-foreign", &decisions);
        assert_eq!(r["outcome"], "refused", "{r}");
        assert!(r["detail"].as_str().unwrap().contains("governs nothing"));
        assert!(matches!(dispatch_hold(&hold_entry(&fx)), HoldState::Held));

        // A coordination-lane row at the right subject is still not law.
        seed_decision(&decisions, "d-coord", "t-0001", "team");
        let r = release_with_decisions(&fx.graph, "d-coord", &decisions);
        assert_eq!(r["outcome"], "refused", "{r}");
        assert!(r["detail"].as_str().unwrap().contains("not superuser lane"));

        // A store that cannot be read never answers "no rulings exist".
        let r = release_with_decisions(
            &fx.graph,
            "d-anything",
            std::path::Path::new("/nonexistent/x-02a1/decisions.jsonl"),
        );
        assert_eq!(r["outcome"], "refused", "{r}");
        assert_eq!(r["exit_code"], 5);
    }

    #[test]
    fn a_hold_naming_a_question_lifts_on_a_ruling_at_that_question() {
        let fx = fixture(json!({}));
        let decisions = fx._dir.path().join("decisions.jsonl");
        let mut set = set_payload(fx.graph.display().to_string());
        set["release_when"] = json!("when question q-02a1 is answered");
        run("hold-set", &set);

        // The named question triggers the guard even on a team-set hold:
        // free text refuses, a chat_attested row AT the question lifts.
        let r = release_with_decisions(&fx.graph, "the answer landed", &decisions);
        assert_eq!(r["outcome"], "refused", "{r}");
        seed_decision(&decisions, "d-answer", "question:q-02a1", "chat_attested");
        let r = release_with_decisions(&fx.graph, "d-answer", &decisions);
        assert_eq!(r["outcome"], "released", "{r}");
        assert!(matches!(dispatch_hold(&hold_entry(&fx)), HoldState::Absent));
    }

    #[test]
    fn set_disarms_a_queued_auto_merge_best_effort() {
        // No gh call here: the receipt records the outcome either way, and a
        // failure never fails the set (fail-open disarm).
        let fx = fixture(json!({"pr_number": 42}));
        let out = run("hold-set", &set_payload(fx.graph.display().to_string()));
        let r: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(r["exit_code"], 0);
        assert_eq!(r["pr"], 42);
        let disarm = r["disarm"].as_str().unwrap();
        assert!(disarm.starts_with("issued") || disarm.starts_with("failed"));
        assert!(matches!(dispatch_hold(&hold_entry(&fx)), HoldState::Held));
    }

    // --- node-level holds: a merge hold works without a plan file ----

    fn fixture_plan_less(extra: Value) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let mut entry = json!({
            "id": "t-0001",
            "slug": "a-plan",
            "cwd": dir.path().display().to_string(),
        });
        if let (Some(base), Some(ex)) = (entry.as_object_mut(), extra.as_object()) {
            for (k, v) in ex {
                base.insert(k.clone(), v.clone());
            }
        }
        let graph = dir.path().join("graph.json");
        crate::graph_store::seed_rows(&graph, &[entry]).unwrap();
        (dir, graph)
    }

    #[test]
    fn plan_less_node_field_contract() {
        let (_dir, graph) = fixture_plan_less(json!({}));
        let out = run("hold-set", &set_payload(graph.display().to_string()));
        let receipt: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(receipt["outcome"], "held", "{out}");
        assert_eq!(receipt["exit_code"], 0);
        assert_eq!(receipt["hold"]["reason"], "condition R");
        let entry = crate::graph_store::read_rows(&graph).unwrap()[0].clone();
        assert_eq!(entry["dispatch_hold"]["set_by"], "team");
        assert!(matches!(dispatch_hold(&entry), HoldState::Held));
        set_twice_refuses_contract();
        release_clears_contract();
        release_unheld_refuses_contract();
    }

    fn set_twice_refuses_contract() {
        let (_dir, graph) = fixture_plan_less(json!({}));
        let g = graph.display().to_string();
        run("hold-set", &set_payload(g.clone()));
        let out = run("hold-set", &set_payload(g));
        let r: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(r["exit_code"], 3, "{out}");
        assert!(out.contains("already held"), "{out}");
        assert!(out.contains("hold release"), "{out}");
    }

    fn release_clears_contract() {
        let (_dir, graph) = fixture_plan_less(json!({}));
        let g = graph.display().to_string();
        run("hold-set", &set_payload(g.clone()));
        let out = run("hold-release", &release_payload(g, "freeze lifted"));
        let r: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(r["outcome"], "released", "{out}");
        assert_eq!(r["hold"]["evidence"], "freeze lifted");
        let entry = crate::graph_store::read_rows(&graph).unwrap()[0].clone();
        assert!(matches!(dispatch_hold(&entry), HoldState::Absent));
    }

    fn release_unheld_refuses_contract() {
        let (_dir, graph) = fixture_plan_less(json!({}));
        let out = run(
            "hold-release",
            &release_payload(graph.display().to_string(), "nothing held"),
        );
        let r: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(r["exit_code"], 3, "{out}");
        assert!(out.contains("no merge hold"), "{out}");
    }
}
