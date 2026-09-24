//! The king_settle arm: one pass over the court payload that mails the owning
//! king when a covered PR settles green and when a covered node merges and
//! closes. It rides the `arm_watch` tick's court read, so the king arms no
//! watch and relaunches nothing: the daemon is the push half, the check-in
//! beat stays the backstop.

use serde_json::Value;
use std::path::{Path, PathBuf};

/// The `fno do pr status` reads one pass may spend. A court with more open
/// PRs waits one more beat per 12.
// ponytail: 12 reads per 300s beat; a court with more open PRs waits one
// more beat per 12.
pub const MAX_PR_READS: usize = 12;

/// The status read's wall budget, the same bound the court read applies.
const STATUS_READ_BUDGET_S: u64 = 30;

/// The mail child's wall budget, the same bound the confirmed notice applies.
const MAIL_BUDGET_S: u64 = 30;

/// One pass's receipt: what the tick row journals.
pub struct SettleOutcome {
    pub mailed: u64,
    pub reads: u64,
    pub note: String,
}

/// The seen key: proof this pass covered the node, so a later beat can see
/// the node leave the fold.
fn seen_key(scope: &str, node: &str) -> String {
    format!("king_settle_seen:{scope}:{node}")
}

/// The settle key: the send-once token per covered node.
fn settle_key(scope: &str, node: &str) -> String {
    format!("king_settle:{scope}:{node}")
}

fn head12(head: &str) -> String {
    head.chars().take(12).collect()
}

/// The pure pass. `graph` reads one PR's node rows, `status` reads one PR's
/// status payload from its node's cwd, and `mail` sends one crown mail; all
/// handed in so a test drives the whole pass. `rotate` staggers which PRs
/// the read cap spends this beat.
pub(crate) fn evaluate(
    payload: &Value,
    store: &Path,
    graph: &dyn Fn(i64) -> Result<Vec<Value>, String>,
    status: &mut dyn FnMut(&Path, i64) -> Result<Value, String>,
    mail: &mut dyn FnMut(&str, &str) -> bool,
    now_unix: u64,
    rotate: u64,
) -> SettleOutcome {
    let mut notes: Vec<String> = Vec::new();
    let mut mailed: u64 = 0;
    let mut reads: u64 = 0;
    let Some(crowns) = payload.get("crowns").and_then(Value::as_array) else {
        return SettleOutcome {
            mailed: 0,
            reads: 0,
            note: "the court payload carries no crowns list".to_string(),
        };
    };
    let mut scopes_seen: Vec<String> = Vec::new();
    // (scope, node, pr, cwd) - the covered set this pass answers for.
    let mut covered: Vec<(String, String, i64, PathBuf)> = Vec::new();
    for crown in crowns {
        let Some(scope) = crown.get("scope").and_then(Value::as_str) else {
            continue;
        };
        let Some(fold) = crown.get("scope_nodes") else {
            notes.push(format!("crown {scope} carries no fold"));
            continue;
        };
        if fold.get("status").and_then(Value::as_str) != Some("ok") {
            let reason = fold
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("the fold did not run");
            notes.push(format!("crown {scope} fold unread: {reason}"));
            continue;
        }
        if !scopes_seen.iter().any(|s| s == scope) {
            scopes_seen.push(scope.to_string());
        }
        let Some(nodes) = fold.get("nodes").and_then(Value::as_array) else {
            continue;
        };
        for node in nodes {
            let owned = node.get("owned").and_then(Value::as_bool) == Some(true);
            let pr = node
                .get("pr_number")
                .and_then(Value::as_i64)
                .filter(|pr| *pr > 0);
            let (Some(node_id), Some(pr), Some(cwd)) = (
                node.get("id").and_then(Value::as_str),
                pr,
                node.get("cwd").and_then(Value::as_str),
            ) else {
                continue;
            };
            if !owned {
                continue;
            }
            covered.push((
                scope.to_string(),
                node_id.to_string(),
                pr,
                PathBuf::from(cwd),
            ));
        }
    }
    // The seen stamp covers every covered node, read or not: the freed pass
    // needs the node's PR number the beat it leaves the fold.
    for (scope, node, pr, _) in &covered {
        crate::operator_notice::mark_once(store, &seen_key(scope, node), &pr.to_string());
    }
    // The green pass: one mail per covered PR that settled green at a head
    // this arm has not mailed. `pr_state` decides first, because a merged PR
    // also reads green.
    let total = covered.len();
    if total > 0 {
        let start = (rotate as usize) % total;
        for offset in 0..total.min(MAX_PR_READS) {
            let (scope, node, pr, cwd) = &covered[(start + offset) % total];
            reads += 1;
            let status_payload = match status(cwd, *pr) {
                Ok(v) => v,
                Err(reason) => {
                    notes.push(format!("PR #{pr} on {node} status unread: {reason}"));
                    continue;
                }
            };
            if status_payload.get("pr_state").and_then(Value::as_str) != Some("OPEN") {
                continue;
            }
            let settled = status_payload.get("settled").and_then(Value::as_bool) == Some(true);
            let green = status_payload.get("verdict").and_then(Value::as_str) == Some("green");
            let head = status_payload
                .get("head")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !settled || !green || head.is_empty() {
                continue;
            }
            let ready = status_payload.get("ready").and_then(Value::as_bool) == Some(true);
            let text = if ready {
                format!(
                    "PR #{pr} on {node} settled green at {}. It is ready with no blockers.",
                    head12(head)
                )
            } else {
                let blockers = status_payload
                    .get("ready_blockers")
                    .and_then(Value::as_array)
                    .map(|list| {
                        list.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                format!(
                    "PR #{pr} on {node} settled green at {}. Blockers: {blockers}.",
                    head12(head)
                )
            };
            let token = format!(
                "green:{pr}@{head}:{}",
                if ready { "ready" } else { "blocked" }
            );
            let key = settle_key(scope, node);
            let scope_for_mail = scope.clone();
            match crate::operator_notice::notify_signal_via(
                store,
                now_unix,
                0,
                &key,
                &token,
                "king settle",
                &text,
                None,
                || mail(&scope_for_mail, &text),
            ) {
                crate::operator_notice::Verdict::Sent => mailed += 1,
                crate::operator_notice::Verdict::SendFailed => notes.push(format!(
                    "PR #{pr} on {node} mail failed; the next beat retries"
                )),
                _ => {}
            }
        }
    }
    // The freed pass: a node this arm saw covered whose scope still folds but
    // which left the covered set. Its graph row decides whether it merged.
    for scope in &scopes_seen {
        let still: Vec<&str> = covered
            .iter()
            .filter(|(s, _, _, _)| s == scope)
            .map(|(_, n, _, _)| n.as_str())
            .collect();
        let prefix = format!("king_settle_seen:{scope}:");
        for key in crate::operator_notice::keys_with_prefix(store, &prefix) {
            let Some(node) = key.rsplit_once(':').map(|(_, node)| node.to_string()) else {
                continue;
            };
            if still.contains(&node.as_str()) {
                continue;
            }
            let Some(pr) = crate::operator_notice::stored_token(store, &key)
                .and_then(|token| token.parse::<i64>().ok())
            else {
                // A key with no readable PR can never resolve; drop it.
                crate::operator_notice::forget_at(store, &key);
                continue;
            };
            match graph(pr) {
                Err(reason) => {
                    notes.push(format!("PR #{pr} on {node} graph unread: {reason}"));
                }
                Ok(rows) => {
                    let closed = rows
                        .iter()
                        .find(|row| row.get("id").and_then(Value::as_str) == Some(node.as_str()))
                        .map(|row| {
                            row.get("status").and_then(Value::as_str) == Some("done")
                                || row.get("merge_status").and_then(Value::as_str) == Some("merged")
                        })
                        .unwrap_or(false);
                    if !closed {
                        // The row reads any other status: the node is no
                        // longer this arm's business.
                        crate::operator_notice::forget_at(store, &key);
                        crate::operator_notice::forget_at(store, &settle_key(scope, &node));
                        continue;
                    }
                    let text = format!(
                        "PR #{pr} on {node} merged and the node closed. Plan the next dispatch."
                    );
                    let key_settle = settle_key(scope, &node);
                    let scope_for_mail = scope.clone();
                    match crate::operator_notice::notify_signal_via(
                        store,
                        now_unix,
                        0,
                        &key_settle,
                        &format!("freed:{pr}"),
                        "king settle",
                        &text,
                        None,
                        || mail(&scope_for_mail, &text),
                    ) {
                        crate::operator_notice::Verdict::Sent => {
                            mailed += 1;
                            crate::operator_notice::forget_at(store, &key);
                            crate::operator_notice::forget_at(store, &key_settle);
                        }
                        crate::operator_notice::Verdict::Deduped => {
                            crate::operator_notice::forget_at(store, &key);
                            crate::operator_notice::forget_at(store, &key_settle);
                        }
                        crate::operator_notice::Verdict::SendFailed
                        | crate::operator_notice::Verdict::RateHeld => {
                            // Keep the seen key: the next beat retries.
                            notes.push(format!(
                                "PR #{pr} on {node} mail failed; the next beat retries"
                            ));
                        }
                    }
                }
            }
        }
    }
    // A scope that left the payload takes its keys with it, the same cleanup
    // the crown alarm's clocks get.
    for key in crate::operator_notice::keys_with_prefix(store, "king_settle_seen:") {
        let departed = key
            .strip_prefix("king_settle_seen:")
            .and_then(|rest| rest.split(':').next())
            .is_some_and(|scope| !scopes_seen.iter().any(|s| s == scope));
        if departed {
            crate::operator_notice::forget_at(store, &key);
        }
    }
    for key in crate::operator_notice::keys_with_prefix(store, "king_settle:") {
        let departed = key
            .strip_prefix("king_settle:")
            .and_then(|rest| rest.split(':').next())
            .is_some_and(|scope| !scopes_seen.iter().any(|s| s == scope));
        if departed {
            crate::operator_notice::forget_at(store, &key);
        }
    }
    SettleOutcome {
        mailed,
        reads,
        note: notes.join("; "),
    }
}

/// The daemon-facing pass over the court payload the arm_watch tick already
/// read. Every child is bounded; a failed read or send names itself in the
/// note and never mails a half answer. The status read rides the shared
/// `run_fno_output` helper, and the mail child resolves the porcelain
/// through `scrape::fno_bin()` - no second resolver lives here.
pub fn run(payload: &Value, config_cwd: &Path, now_unix: u64) -> SettleOutcome {
    let store = crate::operator_notice::notify_signals_path();
    let graph_path = crate::king_board::scope::graph_json_path(config_cwd);
    let graph = |pr: i64| -> Result<Vec<Value>, String> {
        crate::graph_store::read_pr_rows(&graph_path, Some(pr)).map_err(|e| e.to_string())
    };
    let mut status = |cwd: &Path, pr: i64| -> Result<Value, String> {
        let pr_arg = pr.to_string();
        let out = crate::provider_cap_verbs::run_fno_output(
            &["do", "pr", "status", pr_arg.as_str()],
            Some(cwd),
            std::time::Duration::from_secs(STATUS_READ_BUDGET_S),
        )
        .ok_or_else(|| "the status read failed or timed out".to_string())?;
        serde_json::from_str(&out).map_err(|e| format!("the status payload did not parse: {e}"))
    };
    let mut mail = |scope: &str, text: &str| -> bool {
        let fno = crate::scrape::fno_bin();
        let mut cmd = std::process::Command::new(&fno);
        cmd.args([
            "agents",
            "mail",
            "send",
            "--to-king",
            scope,
            "--from-name",
            "king-settle",
            text,
        ])
        .stdin(std::process::Stdio::null());
        crate::bounded_cmd::output_with_timeout(cmd, MAIL_BUDGET_S)
            .map(|out| out.status.success())
            .unwrap_or(false)
    };
    let rotate = now_unix / crate::arm_watch::ARM_WATCH_INTERVAL_S;
    evaluate(
        payload,
        &store,
        &graph,
        &mut status,
        &mut mail,
        now_unix,
        rotate,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};

    const NOW: u64 = 1_788_523_200; // 2026-09-04T12:00 to the second

    fn temp_store(name: &str) -> PathBuf {
        // pid + counter: same-process tests must not share a store.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "fno-king-settle-{}-{}-{name}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    type Log = Rc<RefCell<Vec<String>>>;

    fn mail_log(log: Log) -> impl FnMut(&str, &str) -> bool {
        move |scope: &str, text: &str| {
            log.borrow_mut().push(format!("{scope}: {text}"));
            true
        }
    }

    fn green_status(head: &str, ready: bool) -> Value {
        json!({
            "pr_state": "OPEN",
            "settled": true,
            "verdict": "green",
            "head": head,
            "ready": ready,
            "ready_blockers": if ready { vec![] } else { vec!["review uncovered"] },
        })
    }

    fn crown(scope: &str, nodes: Value) -> Value {
        json!({
            "holder": "sess-1", "level": 1, "scope": scope,
            "status": "manifest-only",
            "scope_nodes": {"status": "ok", "nodes": nodes},
        })
    }

    fn covered_node(id: &str, pr: i64, owned: Value) -> Value {
        json!({"id": id, "owned": owned, "pr_number": pr, "cwd": "/r"})
    }

    /// AC1: one green settle mails once; the second pass dedupes.
    #[test]
    fn a_green_settle_mails_once_and_the_second_pass_is_quiet() {
        let store = temp_store("ac1");
        let payload =
            json!({"crowns": [crown("s1", json!([covered_node("x-1", 7, json!(true))]))]});
        let graph = |_pr: i64| -> Result<Vec<Value>, String> { Ok(vec![]) };
        let mut status = |_cwd: &Path, _pr: i64| -> Result<Value, String> {
            Ok(green_status("abcdef1234567890", true))
        };
        let log = Log::new(RefCell::new(Vec::new()));
        let mut mail = mail_log(Rc::clone(&log));
        let out = evaluate(&payload, &store, &graph, &mut status, &mut mail, NOW, 0);
        assert_eq!(out.mailed, 1);
        assert_eq!(out.reads, 1);
        assert_eq!(log.borrow().len(), 1);
        assert!(log.borrow()[0].contains("s1: PR #7 on x-1 settled green at abcdef123456"));
        assert!(log.borrow()[0].contains("It is ready with no blockers."));
        let out = evaluate(
            &payload,
            &store,
            &graph,
            &mut status,
            &mut mail,
            NOW + 300,
            1,
        );
        assert_eq!(out.mailed, 0);
        assert_eq!(log.borrow().len(), 1);
    }

    /// AC2: a new green head mails again; so does a head whose blockers
    /// cleared.
    #[test]
    fn a_new_head_or_cleared_blockers_mails_again() {
        let store = temp_store("ac2");
        let payload =
            json!({"crowns": [crown("s1", json!([covered_node("x-1", 7, json!(true))]))]});
        let graph = |_pr: i64| -> Result<Vec<Value>, String> { Ok(vec![]) };
        let head = Rc::new(RefCell::new("aaaaaaaaaaaaaaaa".to_string()));
        let ready = Rc::new(RefCell::new(false));
        let h = Rc::clone(&head);
        let r = Rc::clone(&ready);
        let mut status = move |_cwd: &Path, _pr: i64| -> Result<Value, String> {
            Ok(green_status(&h.borrow(), *r.borrow()))
        };
        let log = Log::new(RefCell::new(Vec::new()));
        let mut mail = mail_log(Rc::clone(&log));
        let out = evaluate(&payload, &store, &graph, &mut status, &mut mail, NOW, 0);
        assert_eq!(out.mailed, 1);
        assert!(log.borrow()[0].contains("Blockers: review uncovered."));
        *head.borrow_mut() = "bbbbbbbbbbbbbbbb".to_string();
        let out = evaluate(
            &payload,
            &store,
            &graph,
            &mut status,
            &mut mail,
            NOW + 300,
            1,
        );
        assert_eq!(out.mailed, 1);
        assert_eq!(log.borrow().len(), 2);
        *ready.borrow_mut() = true;
        let out = evaluate(
            &payload,
            &store,
            &graph,
            &mut status,
            &mut mail,
            NOW + 600,
            2,
        );
        assert_eq!(out.mailed, 1);
        assert_eq!(log.borrow().len(), 3);
        assert!(log.borrow()[2].contains("It is ready with no blockers."));
    }

    /// AC3: a merged PR also reads green, but pr_state decides first.
    #[test]
    fn a_merged_pr_mails_nothing() {
        let store = temp_store("ac3");
        let payload =
            json!({"crowns": [crown("s1", json!([covered_node("x-1", 7, json!(true))]))]});
        let graph = |_pr: i64| -> Result<Vec<Value>, String> { Ok(vec![]) };
        let mut status = |_cwd: &Path, _pr: i64| -> Result<Value, String> {
            let mut v = green_status("abcdef1234567890", true);
            v["pr_state"] = json!("MERGED");
            Ok(v)
        };
        let log = Log::new(RefCell::new(Vec::new()));
        let mut mail = mail_log(Rc::clone(&log));
        let out = evaluate(&payload, &store, &graph, &mut status, &mut mail, NOW, 0);
        assert_eq!(out.mailed, 0);
        assert!(log.borrow().is_empty());
    }

    /// AC4: a covered node that left the fold with a done row mails the
    /// freed notice and both keys leave the store.
    #[test]
    fn a_node_that_merged_and_closed_mails_freed_and_forgets() {
        let store = temp_store("ac4");
        crate::operator_notice::mark_once(&store, &seen_key("s1", "x-1"), "7");
        let payload = json!({"crowns": [crown("s1", json!([]))]});
        let graph = |_pr: i64| -> Result<Vec<Value>, String> {
            Ok(vec![json!({"id": "x-1", "status": "done"})])
        };
        let mut status =
            |_cwd: &Path, _pr: i64| -> Result<Value, String> { Err("never read".to_string()) };
        let log = Log::new(RefCell::new(Vec::new()));
        let mut mail = mail_log(Rc::clone(&log));
        let out = evaluate(&payload, &store, &graph, &mut status, &mut mail, NOW, 0);
        assert_eq!(out.mailed, 1);
        assert!(log.borrow()[0].contains("PR #7 on x-1 merged and the node closed"));
        assert!(crate::operator_notice::stored_token(&store, &seen_key("s1", "x-1")).is_none());
        assert!(crate::operator_notice::stored_token(&store, &settle_key("s1", "x-1")).is_none());
    }

    /// AC5: unowned nodes and an unresolved fold read nothing and mail
    /// nothing; the note names the unresolved crown.
    #[test]
    fn unowned_and_unresolved_are_skipped_and_named() {
        let store = temp_store("ac5");
        let payload = json!({"crowns": [
            crown("s1", json!([covered_node("x-1", 7, json!(false)), covered_node("x-2", 8, Value::Null)])),
            json!({
                "holder": "sess-2", "level": 1, "scope": "s2",
                "status": "manifest-only",
                "scope_nodes": {"status": "unresolved", "reason": "scope does not compile"},
            }),
        ]});
        let graph = |_pr: i64| -> Result<Vec<Value>, String> { Ok(vec![]) };
        let mut status = |_cwd: &Path, _pr: i64| -> Result<Value, String> {
            Ok(green_status("abcdef1234567890", true))
        };
        let log = Log::new(RefCell::new(Vec::new()));
        let mut mail = mail_log(Rc::clone(&log));
        let out = evaluate(&payload, &store, &graph, &mut status, &mut mail, NOW, 0);
        assert_eq!(out.mailed, 0);
        assert_eq!(out.reads, 0);
        assert!(log.borrow().is_empty());
        assert!(out.note.contains("s2"));
    }

    /// AC6: a failed send stores no token, so the next pass sends again.
    #[test]
    fn a_failed_send_retries_the_next_pass() {
        let store = temp_store("ac6");
        let payload =
            json!({"crowns": [crown("s1", json!([covered_node("x-1", 7, json!(true))]))]});
        let graph = |_pr: i64| -> Result<Vec<Value>, String> { Ok(vec![]) };
        let mut status = |_cwd: &Path, _pr: i64| -> Result<Value, String> {
            Ok(green_status("abcdef1234567890", true))
        };
        let log = Log::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&log);
        let mut mail = move |scope: &str, text: &str| -> bool {
            sink.borrow_mut().push(format!("{scope}: {text}"));
            false
        };
        let out = evaluate(&payload, &store, &graph, &mut status, &mut mail, NOW, 0);
        assert_eq!(out.mailed, 0);
        let out = evaluate(
            &payload,
            &store,
            &graph,
            &mut status,
            &mut mail,
            NOW + 300,
            1,
        );
        assert_eq!(out.mailed, 0);
        assert_eq!(
            log.borrow().len(),
            2,
            "the send retried instead of deduping"
        );
    }

    /// A failed status read names itself and sends nothing.
    #[test]
    fn a_failed_status_read_is_named_and_mails_nothing() {
        let store = temp_store("ac-note");
        let payload =
            json!({"crowns": [crown("s1", json!([covered_node("x-1", 7, json!(true))]))]});
        let graph = |_pr: i64| -> Result<Vec<Value>, String> { Ok(vec![]) };
        let mut status =
            |_cwd: &Path, _pr: i64| -> Result<Value, String> { Err("timed out".to_string()) };
        let log = Log::new(RefCell::new(Vec::new()));
        let mut mail = mail_log(Rc::clone(&log));
        let out = evaluate(&payload, &store, &graph, &mut status, &mut mail, NOW, 0);
        assert_eq!(out.mailed, 0);
        assert!(out.note.contains("PR #7 on x-1 status unread: timed out"));
    }

    /// A scope that left the payload takes its keys with it.
    #[test]
    fn a_departed_scope_forgets_its_keys() {
        let store = temp_store("ac-departed");
        crate::operator_notice::mark_once(&store, &seen_key("gone", "x-9"), "3");
        crate::operator_notice::mark_once(&store, &settle_key("gone", "x-9"), "green:3@aa:ready");
        crate::operator_notice::mark_once(&store, &seen_key("s1", "x-1"), "7");
        let payload =
            json!({"crowns": [crown("s1", json!([covered_node("x-1", 7, json!(true))]))]});
        let graph = |_pr: i64| -> Result<Vec<Value>, String> { Ok(vec![]) };
        let mut status = |_cwd: &Path, _pr: i64| -> Result<Value, String> {
            Ok(green_status("abcdef1234567890", true))
        };
        let log = Log::new(RefCell::new(Vec::new()));
        let mut mail = mail_log(Rc::clone(&log));
        let out = evaluate(&payload, &store, &graph, &mut status, &mut mail, NOW, 0);
        assert_eq!(out.mailed, 1);
        assert!(crate::operator_notice::stored_token(&store, &seen_key("gone", "x-9")).is_none());
        assert!(crate::operator_notice::stored_token(&store, &settle_key("gone", "x-9")).is_none());
        assert_eq!(
            crate::operator_notice::stored_token(&store, &seen_key("s1", "x-1")),
            Some("7".to_string())
        );
    }

    /// The read cap: a court past 12 covered PRs reads 12 and rotates.
    #[test]
    fn the_read_cap_spends_twelve_reads_and_rotates() {
        let store = temp_store("ac-cap");
        let nodes: Vec<Value> = (0..15)
            .map(|i| covered_node(&format!("x-{i}"), 100 + i, json!(true)))
            .collect();
        let payload = json!({"crowns": [crown("s1", json!(nodes))]});
        let graph = |_pr: i64| -> Result<Vec<Value>, String> { Ok(vec![]) };
        let seen = Rc::new(RefCell::new(Vec::<i64>::new()));
        let sink = Rc::clone(&seen);
        let mut status = move |_cwd: &Path, pr: i64| -> Result<Value, String> {
            sink.borrow_mut().push(pr);
            Err("counted".to_string())
        };
        let log = Log::new(RefCell::new(Vec::new()));
        let mut mail = mail_log(Rc::clone(&log));
        let out = evaluate(&payload, &store, &graph, &mut status, &mut mail, NOW, 3);
        assert_eq!(out.reads, 12);
        assert_eq!(seen.borrow().len(), 12);
        assert_eq!(
            seen.borrow()[0],
            103,
            "rotate=3 starts at the fourth covered node"
        );
        let out = evaluate(&payload, &store, &graph, &mut status, &mut mail, NOW, 15);
        assert_eq!(out.reads, 12);
        assert_eq!(seen.borrow()[12], 100, "rotate wraps past the end");
    }
}
