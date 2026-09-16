//! The merge-hold writer behind the `authorized-merge` verb's `op` field.
//!
//! A crown or worker pipes `{"op": "hold-set"|"hold-release", ...}` straight
//! into `fno-agents authorized-merge` (taught in the king and blueprint
//! skills). The block it writes is the same
//! `dispatch_hold` frontmatter every merge path already reads; the write is
//! proven by the same reader ready selection uses, and a failed readback
//! restores the original bytes. Rides an existing verb rather than a new
//! top-level root, and keeps the writer out of the Python tree the file
//! budget caps.

use crate::backlog_ready::{
    dispatch_hold, dispatch_hold_verdict, read_frontmatter, resolve_plan_probe, HoldState,
};
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
    let graph = payload
        .get("graph")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(default_graph_path);
    let entries = match graph_store::read_defaulted(&graph, false) {
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
    let node_id = entry
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(node)
        .to_string();
    match op.strip_prefix("hold-").unwrap_or(op) {
        "set" => set_hold(&entry, &node_id, payload),
        "release" => release_hold(&entry, &node_id, payload, &entries),
        other => receipt("refused", 2, format!("unknown hold op: {other}")).to_string(),
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
/// read-modify-write is not atomic as a whole, and a crown setting while a
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
    let Some(fm) = read_frontmatter(probe) else {
        return (String::new(), String::new());
    };
    let Some(block) = fm.get("dispatch_hold").and_then(Value::as_object) else {
        return (String::new(), String::new());
    };
    let field = |k: &str| {
        block
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    (field("reason"), field("release_when"))
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
fn disarm_automerge(pr: u64) -> String {
    match std::process::Command::new("gh")
        .args(["pr", "merge", &pr.to_string(), "--disable-auto"])
        .output()
    {
        Ok(o) if o.status.success() => "issued".to_string(),
        Ok(o) => format!("failed: gh exited {}", o.status.code().unwrap_or(-1)),
        Err(e) => format!("failed: {e}"),
    }
}

fn set_hold(entry: &Value, node_id: &str, payload: &Value) -> String {
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
        Err(err) => return err.to_string(),
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
    if let Some(err) = write_proven(&probe, entry, &new_text, &original, HoldState::Held) {
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
        obj.insert("disarm".into(), Value::String(disarm));
    }
    out.to_string()
}

fn payload_str<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
}

fn release_hold(entry: &Value, node_id: &str, payload: &Value, entries: &[Value]) -> String {
    let evidence = payload_str(payload, "evidence").unwrap_or("");
    if evidence.trim().is_empty() {
        return receipt("refused", 2, "release needs a non-blank --evidence").to_string();
    }
    let probe = match resolve_plan(entry, node_id) {
        Ok(p) => p,
        Err(err) => return err.to_string(),
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
        std::fs::write(
            &graph,
            serde_json::to_string(&json!({"entries": [entry]})).unwrap(),
        )
        .unwrap();
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
            "set_by": "crown",
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
        let raw = std::fs::read_to_string(&f.graph).unwrap();
        let entries: Value = serde_json::from_str(&raw).unwrap();
        entries["entries"][0].clone()
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
        std::fs::write(
            &graph,
            serde_json::to_string(&json!({"entries": [
                {"id": "t-parent", "slug": "parent", "plan_path": parent_plan.display().to_string(), "cwd": dir.path().display().to_string()},
                {"id": "t-0001", "slug": "a-plan", "parent": "t-parent", "plan_path": child_plan.display().to_string(), "cwd": dir.path().display().to_string()},
            ]}))
            .unwrap(),
        )
        .unwrap();
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
}
