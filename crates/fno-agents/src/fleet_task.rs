//! The fleet-task channel: a machine chore is not a question.
//!
//! A writer whose row already names the command that clears it — a heal
//! rebase conflict, a pr-nudge, a watchdog hold — files a `fleet_task` row
//! in the inbox index ([`crate::provider_cap::questions_path`]) instead of
//! an `operator_question`. Every reader keyed on the question type falls
//! out of the new type untouched: the Python fold, the SessionStart block,
//! the attention projection, the notify arm, the stop gate, the spawn gate.
//! A king reads open tasks on the board's `fleet_task` queue, report-only.
//!
//! Identity is lane + key + cwd: one heal process serves several repos and
//! PR numbers repeat across them, so a task filed for another root is never
//! this root's duplicate and never closed by this root's reconcile.
//!
//! [`run_fleet_task`] is the transport-only arm (`fno-agents fleet-task`,
//! wired in bin/client.rs): the Python reconcile lanes reach [`reconcile`]
//! through one `verb_call`. It registers no client verb — the shrink law
//! allows none.

use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;

/// The question prefixes this plan moved off the question channel. A match
/// is a PREFIX of the question text, never a substring: an agent ask that
/// quotes `heal: PR 9` mid-sentence is a real question and stays one.
pub const LEGACY_MARKERS: [&str; 7] = [
    "heal: PR ",
    "pr-nudge: PR #",
    "[watchdog-stale:",
    "[watchdog-friction:",
    "[reap-hold:",
    "[watchdog-unfinished-work:",
    "[king-escalation:",
];

/// One open fleet task, folded from the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub lane: String,
    pub key: String,
    pub cwd: String,
    pub text: String,
    pub run: String,
    pub node: String,
    pub ts: String,
}

/// The outcome of [`file_once`]: the id is the open task's either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filed {
    New(String),
    Duplicate(String),
}

impl Filed {
    pub fn id(&self) -> &str {
        match self {
            Filed::New(id) | Filed::Duplicate(id) => id,
        }
    }
}

fn new_task_id() -> String {
    let mut buf = [0u8; 4];
    getrandom::fill(&mut buf).expect("OS CSPRNG unavailable");
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    format!("ft-{hex}")
}

fn now_ts() -> String {
    crate::provider_cap::epoch_to_rfc3339(crate::provider_cap::now_epoch_secs())
}

fn append(store: &Path, kind: &str, data: Value) {
    crate::provider_cap::append_questions_row(
        store,
        &json!({"ts": now_ts(), "type": kind, "source": "daemon", "data": data}),
    );
}

fn task_from(data: &Value, ts: &str) -> Option<Task> {
    let id = data.get("task_id").and_then(Value::as_str)?;
    let s = |k: &str| {
        data.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    Some(Task {
        id: id.to_string(),
        lane: s("lane"),
        key: s("key"),
        cwd: s("cwd"),
        text: s("text"),
        run: s("run"),
        node: s("node"),
        ts: ts.to_string(),
    })
}

fn read_store(store: &Path) -> Result<String, String> {
    match std::fs::read_to_string(store) {
        Ok(raw) => Ok(raw),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!(
            "fleet task store unreadable at {}: {e}",
            store.display()
        )),
    }
}

fn fold_open_tasks(raw: &str) -> Vec<Task> {
    let mut open: Vec<Task> = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() || !line.contains("fleet_task") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        let data = v.get("data").cloned().unwrap_or_else(|| json!({}));
        match kind {
            "fleet_task" => {
                if let Some(t) = task_from(&data, v.get("ts").and_then(Value::as_str).unwrap_or(""))
                {
                    if !open.iter().any(|t2| t2.id == t.id) {
                        open.push(t);
                    }
                }
            }
            "fleet_task_closed" => {
                if let Some(id) = data.get("task_id").and_then(Value::as_str) {
                    open.retain(|t| t.id != id);
                }
            }
            _ => {}
        }
    }
    open
}

/// Every open task, oldest first: `fleet_task` minus `fleet_task_closed`.
/// A malformed line is skipped. A missing store reads empty; a store that
/// exists and cannot be read is `Err` — a silently empty fold would re-file
/// every task on the next tick (AC3-ERR).
pub fn open_tasks(store: &Path) -> Result<Vec<Task>, String> {
    let raw = read_store(store)?;
    Ok(fold_open_tasks(&raw))
}

/// File once per (lane, key, cwd): a second call with the same identity
/// returns the open task's id and writes nothing, so a 600s heal tick can
/// never flood the store (AC1-HP). A missing store is created (AC3-ERR).
pub fn file_once(
    store: &Path,
    lane: &str,
    key: &str,
    cwd: &str,
    text: &str,
    run: Option<&str>,
    node: Option<&str>,
) -> Result<Filed, String> {
    let open = open_tasks(store)?;
    if let Some(t) = open
        .iter()
        .find(|t| t.lane == lane && t.key == key && t.cwd == cwd)
    {
        return Ok(Filed::Duplicate(t.id.clone()));
    }
    let id = new_task_id();
    let mut data = json!({
        "task_id": id,
        "lane": lane,
        "key": key,
        "cwd": cwd,
        "text": text,
    });
    let obj = data.as_object_mut().expect("object literal");
    if let Some(r) = run {
        obj.insert("run".to_string(), json!(r));
    }
    if let Some(n) = node {
        obj.insert("node".to_string(), json!(n));
    }
    append(store, "fleet_task", data);
    Ok(Filed::New(id))
}

fn close_by_id(store: &Path, id: &str, reason: &str, closed_by: &str) {
    append(
        store,
        "fleet_task_closed",
        json!({"task_id": id, "reason": reason, "closed_by": closed_by}),
    );
}

/// Close every open task with this exact identity (lane, key, cwd).
pub fn close(
    store: &Path,
    lane: &str,
    key: &str,
    cwd: &str,
    reason: &str,
    closed_by: &str,
) -> Result<(), String> {
    for t in open_tasks(store)? {
        if t.lane == lane && t.key == key && t.cwd == cwd {
            close_by_id(store, &t.id, reason, closed_by);
        }
    }
    Ok(())
}

/// The lane reconcile the Python channels used to run inline (the port of
/// `stale_escalate.reconcile_channel`, minus the answered branch: a task
/// never asks a human, so no answer can pend). The lane's set is the open
/// tasks with this lane and cwd. An empty set closes the set (`set-empty`)
/// and returns `closed`, or `none` when nothing was open. The same key open
/// returns `duplicate` and closes the lane's other open tasks. A new key
/// appends FIRST, then closes the others (`superseded`): a failed close
/// costs a duplicate, never an empty lane.
pub fn reconcile(
    store: &Path,
    lane: &str,
    key: &str,
    cwd: &str,
    text: &str,
    run: &str,
    empty: bool,
) -> Result<(String, String), String> {
    let open = open_tasks(store)?;
    let lane_set: Vec<&Task> = open
        .iter()
        .filter(|t| t.lane == lane && t.cwd == cwd)
        .collect();
    if empty {
        let mut any = false;
        for t in &lane_set {
            close_by_id(store, &t.id, "set-empty", lane);
            any = true;
        }
        return Ok((
            if any {
                "closed".to_string()
            } else {
                "none".to_string()
            },
            String::new(),
        ));
    }
    if let Some(t) = lane_set.iter().copied().find(|t| t.key == key) {
        for other in &lane_set {
            if other.id != t.id {
                close_by_id(store, &other.id, "superseded", lane);
            }
        }
        return Ok(("duplicate".to_string(), t.id.clone()));
    }
    let id = new_task_id();
    append(
        store,
        "fleet_task",
        json!({
            "task_id": id,
            "lane": lane,
            "key": key,
            "cwd": cwd,
            "text": text,
            "run": run,
        }),
    );
    for other in &lane_set {
        close_by_id(store, &other.id, "superseded", lane);
    }
    Ok(("asked".to_string(), id))
}

/// Open task ids whose `node` reads a terminal rung (`done` / `superseded`),
/// for the sweep to close. A task with no node never auto-closes: only a
/// node closure proves the chore moot (AC11-HP). A heal check that went
/// green can outlive its task until its lane reconciles it.
pub fn node_closed_task_ids(
    raw: &str,
    statuses: &std::collections::BTreeMap<String, String>,
) -> Vec<String> {
    const CLOSED_RUNGS: [&str; 2] = ["done", "superseded"];
    fold_open_tasks(raw)
        .into_iter()
        .filter(|t| {
            statuses
                .get(&t.node)
                .is_some_and(|s| CLOSED_RUNGS.contains(&s.as_str()))
        })
        .map(|t| t.id)
        .collect()
}

/// Open operator_question ids whose text STARTS WITH a moved marker: the
/// legacy machine rows this plan retires (AC12-HP). An agent ask that
/// contains a marker after its first word stays open (AC13-EDGE), and the
/// markers this plan did not move stay open (AC14-EDGE).
pub fn legacy_question_ids(raw: &str) -> Vec<String> {
    let mut asked: Vec<(String, String)> = Vec::new();
    let mut closed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in raw.lines() {
        if line.trim().is_empty() || !line.contains("operator_question") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        let data = v.get("data").cloned().unwrap_or_else(|| json!({}));
        match kind {
            "operator_question" => {
                if let (Some(qid), Some(q)) = (
                    data.get("question_id").and_then(Value::as_str),
                    data.get("question").and_then(Value::as_str),
                ) {
                    if !asked.iter().any(|(id, _)| id == qid) {
                        asked.push((qid.to_string(), q.to_string()));
                    }
                }
            }
            "operator_question_closed" => {
                if let Some(qid) = data.get("question_id").and_then(Value::as_str) {
                    closed.insert(qid.to_string());
                }
            }
            _ => {}
        }
    }
    asked
        .into_iter()
        .filter(|(qid, q)| !closed.contains(qid) && LEGACY_MARKERS.iter().any(|m| q.starts_with(m)))
        .map(|(qid, _)| qid)
        .collect()
}

/// `fno-agents fleet-task`: the hidden binary-direct transport the Python
/// reconcile lanes ride (`verb_call`). One JSON request on stdin,
/// `{"op":"reconcile","lane","key","cwd","text","run","empty"}`; one JSON
/// answer on stdout, `{"outcome","id"}`. Exit 2 on bad input; the store
/// resolves through `AgentsHome::from_env()`.
pub fn run_fleet_task(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("usage: fno-agents fleet-task (one JSON request on stdin: op=reconcile)");
        return 0;
    }
    if !args.is_empty() {
        eprintln!("fno-agents fleet-task: unexpected arguments; the request rides stdin");
        return 2;
    }
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        eprintln!("fno-agents fleet-task: could not read stdin");
        return 2;
    }
    let Ok(req) = serde_json::from_str::<Value>(&input) else {
        eprintln!("fno-agents fleet-task: bad request");
        return 2;
    };
    match handle_request(&req) {
        Ok(answer) => {
            println!("{answer}");
            0
        }
        Err(msg) => {
            eprintln!("fno-agents fleet-task: {msg}");
            2
        }
    }
}

fn handle_request(req: &Value) -> Result<String, String> {
    let op = req.get("op").and_then(Value::as_str).unwrap_or("");
    if op != "reconcile" {
        return Err(format!("unknown op {op:?}; expected reconcile"));
    }
    let s = |k: &str| req.get(k).and_then(Value::as_str).unwrap_or("");
    let home = crate::paths::AgentsHome::from_env();
    let store = crate::provider_cap::questions_path(&home);
    let (outcome, id) = reconcile(
        &store,
        s("lane"),
        s("key"),
        s("cwd"),
        s("text"),
        s("run"),
        req.get("empty").and_then(Value::as_bool).unwrap_or(false),
    )?;
    Ok(json!({"outcome": outcome, "id": id}).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "ft-{}-{}-{name}",
            std::process::id(),
            now_epoch_secs()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base.join("questions.jsonl")
    }

    fn now_epoch_secs() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    #[test]
    fn ac1_file_once_twice_holds_one_row_and_duplicates() {
        let store = temp_store("ac1");
        let first = file_once(
            &store,
            "heal",
            "PR 7 rebase conflict",
            "/r",
            "text",
            Some("run"),
            None,
        )
        .unwrap();
        let second = file_once(
            &store,
            "heal",
            "PR 7 rebase conflict",
            "/r",
            "text",
            Some("run"),
            None,
        )
        .unwrap();
        assert!(matches!(first, Filed::New(_)));
        assert_eq!(second, Filed::Duplicate(first.id().to_string()));
        let open = open_tasks(&store).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].lane, "heal");
        assert_eq!(open[0].run, "run");
    }

    #[test]
    fn ac2_fold_reads_only_the_fleet_task_types() {
        let store = temp_store("ac2");
        let raw = format!(
            "{}\n{}\n{}\n",
            r#"{"ts":"t","type":"operator_question","source":"daemon","data":{"question_id":"q-1","question":"real question"}}"#,
            r#"{"ts":"t","type":"fleet_task","source":"daemon","data":{"task_id":"ft-aaaabbbb","lane":"heal","key":"k","cwd":"/r","text":"chore"}}"#,
            r#"{"ts":"t","type":"garbage"}"#,
        );
        std::fs::write(&store, &raw).unwrap();
        let open = open_tasks(&store).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, "ft-aaaabbbb");
    }

    #[test]
    fn ac3_err_unreadable_store_errors_and_missing_store_is_created() {
        let store = temp_store("ac3-dir");
        // A DIRECTORY standing in for the store: the path exists, reads fail.
        std::fs::create_dir_all(&store).unwrap();
        let err = file_once(&store, "heal", "k", "/r", "t", None, None).unwrap_err();
        assert!(
            err.contains(&store.display().to_string()),
            "naming path: {err}"
        );
        let missing = temp_store("ac3-missing");
        let filed = file_once(&missing, "heal", "k", "/r", "t", None, None).unwrap();
        assert!(matches!(filed, Filed::New(_)));
        assert!(missing.is_file());
    }

    #[test]
    fn ac7_reconcile_life_cycle_asked_duplicate_superseded_closed() {
        let store = temp_store("ac7");
        let (outcome, id1) =
            reconcile(&store, "watchdog-stale", "k1", "/r", "text a", "run", false).unwrap();
        assert_eq!(outcome, "asked");
        let (outcome, id2) =
            reconcile(&store, "watchdog-stale", "k1", "/r", "text a", "run", false).unwrap();
        assert_eq!(outcome, "duplicate");
        assert_eq!(id1, id2);
        let (outcome, id3) =
            reconcile(&store, "watchdog-stale", "k2", "/r", "text b", "run", false).unwrap();
        assert_eq!(outcome, "asked");
        let open = open_tasks(&store).unwrap();
        assert_eq!(open.len(), 1, "superseded closed: {open:?}");
        assert_eq!(open[0].id, id3);
        let (outcome, _) = reconcile(&store, "watchdog-stale", "", "/r", "", "", true).unwrap();
        assert_eq!(outcome, "closed");
        assert!(open_tasks(&store).unwrap().is_empty());
        let (outcome, _) = reconcile(&store, "watchdog-stale", "", "/r", "", "", true).unwrap();
        assert_eq!(outcome, "none");
    }

    #[test]
    fn reconcile_scopes_by_cwd_so_another_roots_task_survives() {
        let store = temp_store("cwd-scope");
        file_once(
            &store,
            "heal",
            "PR 7 rebase conflict",
            "/other",
            "t",
            None,
            None,
        )
        .unwrap();
        let (outcome, _) = reconcile(
            &store,
            "heal",
            "PR 7 rebase conflict",
            "/r",
            "t",
            "run",
            false,
        )
        .unwrap();
        assert_eq!(outcome, "asked");
        assert_eq!(open_tasks(&store).unwrap().len(), 2);
    }

    #[test]
    fn ac11_node_closed_task_ids_names_only_terminal_rungs() {
        let raw = concat!(
            r#"{"ts":"t","type":"fleet_task","source":"daemon","data":{"task_id":"ft-done","lane":"heal","key":"k","cwd":"/r","node":"x-1"}}"#,
            "\n",
            r#"{"ts":"t","type":"fleet_task","source":"daemon","data":{"task_id":"ft-wip","lane":"heal","key":"k2","cwd":"/r","node":"x-2"}}"#,
            "\n",
            r#"{"ts":"t","type":"fleet_task","source":"daemon","data":{"task_id":"ft-nobody","lane":"heal","key":"k3","cwd":"/r"}}"#,
            "\n",
        );
        let mut statuses = std::collections::BTreeMap::new();
        statuses.insert("x-1".to_string(), "done".to_string());
        statuses.insert("x-2".to_string(), "in_progress".to_string());
        assert_eq!(
            node_closed_task_ids(raw, &statuses),
            vec!["ft-done".to_string()]
        );
    }

    #[test]
    fn ac12_legacy_ids_match_prefixes_and_never_substrings() {
        let raw = concat!(
            r#"{"ts":"t","type":"operator_question","source":"daemon","data":{"question_id":"q-heal","question":"heal: PR 9 rebase conflict. Resolve with x"}}"#,
            "\n",
            r#"{"ts":"t","type":"operator_question","source":"daemon","data":{"question_id":"q-agent","question":"PR 9 failed and the log quotes heal: PR 9 mid-sentence"}}"#,
            "\n",
            r#"{"ts":"t","type":"operator_question","source":"daemon","data":{"question_id":"q-branch","question":"[session-transition-branch: keep or clean?]"}}"#,
            "\n",
            r#"{"ts":"t","type":"operator_question","source":"daemon","data":{"question_id":"q-hold","question":"[reap-hold: x] held"}}"#,
            "\n",
            r#"{"ts":"t","type":"operator_question","source":"daemon","data":{"question_id":"q-closed","question":"heal: PR 1 old"}}"#,
            "\n",
            r#"{"ts":"t","type":"operator_question_closed","source":"daemon","data":{"question_id":"q-closed","answer":"","reason":"moved-to-fleet-task"}}"#,
            "\n",
        );
        assert_eq!(
            legacy_question_ids(raw),
            vec!["q-heal".to_string(), "q-hold".to_string()]
        );
    }
}
