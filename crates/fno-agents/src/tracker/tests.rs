//! Recorded-response tests for the tracker seam (mounted by `mod.rs`).

use super::github::GitHubTracker;
use super::linear::LinearTracker;
use super::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A hermetic env for tests that touch HOME-derived paths: FNO_CONFIG names an
/// empty file, so no real config resolves; HOME lands in the temp dir. Hold
/// the crate's env lock first so sibling tests do not read the overrides.
/// The returned guard restores the previous values on drop, so the overrides
/// never leak into sibling tests under parallel execution.
fn hermetic_env(tmp: &std::path::Path) -> EnvGuard {
    let empty = tmp.join("empty.toml");
    std::fs::write(&empty, "").unwrap();
    let saved = [
        ("FNO_CONFIG", std::env::var("FNO_CONFIG").ok()),
        ("HOME", std::env::var("HOME").ok()),
        ("FNO_HOME", std::env::var("FNO_HOME").ok()),
        (
            "FNO_NO_CANONICAL_CONFIG",
            std::env::var("FNO_NO_CANONICAL_CONFIG").ok(),
        ),
    ];
    std::env::set_var("FNO_CONFIG", &empty);
    std::env::set_var("HOME", tmp);
    std::env::set_var("FNO_HOME", tmp);
    std::env::set_var("FNO_NO_CANONICAL_CONFIG", "1");
    EnvGuard(saved)
}

struct EnvGuard([(&'static str, Option<String>); 4]);

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.0.iter() {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

struct FakeGh {
    view: (i32, String, String),
    list_open: (i32, String, String),
    list_closed: (i32, String, String),
}

impl github::GhRun for FakeGh {
    fn run(&self, args: &[String]) -> Result<(i32, String, String), String> {
        Ok(match (args[0].as_str(), args[1].as_str()) {
            ("issue", "view") => self.view.clone(),
            ("issue", "list") => {
                let closed = args
                    .iter()
                    .zip(args.iter().skip(1))
                    .any(|(a, v)| a == "--state" && v == "closed");
                if closed {
                    self.list_closed.clone()
                } else {
                    self.list_open.clone()
                }
            }
            _ => (1, String::new(), "unexpected gh call".into()),
        })
    }
}

/// A full in-memory tracker: candidates plus their sidecar maps.
struct FakeTracker {
    cands: Vec<Candidate>,
    sidecars: HashMap<String, serde_json::Map<String, Value>>,
    fail_list_open: bool,
}

impl Tracker for FakeTracker {
    fn name(&self) -> &str {
        "fake"
    }

    fn read(&self, id: &str) -> Result<TrackerNode, TrackerError> {
        self.cands
            .iter()
            .find(|c| c.node.id == id)
            .map(|c| c.node.clone())
            .ok_or_else(|| TrackerError::NotFound(id.to_string()))
    }

    fn list_open(&self) -> Result<Vec<Candidate>, TrackerError> {
        if self.fail_list_open {
            return Err(TrackerError::Backend(
                "gh issue list failed for o/r: network down".into(),
            ));
        }
        Ok(self.cands.clone())
    }

    fn list_closed_since(&self, _days: u32) -> Option<Result<Vec<Candidate>, TrackerError>> {
        Some(Ok(Vec::new()))
    }

    fn sidecar(&self, id: &str) -> Result<serde_json::Map<String, Value>, TrackerError> {
        Ok(self.sidecars.get(id).cloned().unwrap_or_default())
    }

    fn close(&self, _id: &str) -> Result<(), TrackerError> {
        Ok(())
    }
}

fn cand(id: &str, title: &str, blocked_by: &[&str]) -> Candidate {
    Candidate {
        node: TrackerNode {
            id: id.into(),
            title: Some(title.into()),
            state: State::Open,
            parent: None,
            blocked_by: blocked_by.iter().map(|s| (*s).into()).collect(),
            details: Some(format!("body of {id}")),
            url: Some(format!("https://x/{id}")),
            size: None,
        },
        priority: "p2".into(),
        rank: None,
        created_at: Some("2026-01-01T00:00:00Z".into()),
        closed_at: None,
    }
}

#[test]
fn ac1_snapshot_joins_sidecar_over_a_recorded_gh() {
    let dir = tempfile::tempdir().unwrap();
    // hermetic_env mutates process-global env; hold the crate's env lock so
    // parallel env-holding tests (ac2, a_scope_switch) cannot interleave.
    let _lock = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _env = hermetic_env(dir.path());
    // Sidecar file for o/r#1 with plan_path, cwd and two sessions.
    let sidecar_dir = dir.path().join(".fno/sidecar");
    std::fs::create_dir_all(&sidecar_dir).unwrap();
    let sc = json!({
        "id": "o/r#1",
        "plan_path": "/plans/one.md",
        "cwd": "/repo",
        "sessions": [{"phase": "execute", "session_id": "s1"}, {"phase": "execute", "session_id": "s2"}],
    });
    std::fs::write(
        sidecar_dir.join(format!("{}.json", crate::claims::encode_key("o/r#1"))),
        sc.to_string(),
    )
    .unwrap();
    let list_payload = json!([
        {"number": 1, "title": "First", "state": "OPEN",
         "createdAt": "2026-01-02T00:00:00Z", "body": "b1", "url": "u1"},
        {"number": 2, "title": "Second", "state": "OPEN",
         "createdAt": "2026-01-03T00:00:00Z", "body": "b2", "url": "u2"},
    ]);
    let gh = FakeGh {
        view: (0, String::new(), String::new()),
        list_open: (0, list_payload.to_string(), String::new()),
        list_closed: (0, "[]".into(), String::new()),
    };
    let t = GitHubTracker::new(Some("o/r".into()), Box::new(gh));
    let doc = snapshot::build(&t, "github").unwrap();
    let entries = doc.get("entries").and_then(Value::as_array).unwrap();
    assert_eq!(entries.len(), 2);
    let e1 = entries
        .iter()
        .find(|e| e.get("id").and_then(Value::as_str) == Some("o/r#1"))
        .unwrap();
    assert_eq!(e1["title"], "First");
    assert_eq!(e1["state"], "open");
    assert_eq!(e1["created_at"], "2026-01-02T00:00:00Z");
    assert_eq!(e1["details"], "b1");
    assert_eq!(e1["url"], "u1");
    assert_eq!(e1["plan_path"], "/plans/one.md");
    assert_eq!(e1["cwd"], "/repo");
    assert_eq!(e1["sessions"].as_array().unwrap().len(), 2);
    let e2 = entries
        .iter()
        .find(|e| e.get("id").and_then(Value::as_str) == Some("o/r#2"))
        .unwrap();
    assert_eq!(e2["title"], "Second");
    assert_eq!(e2["status"], "idea");
}

#[test]
fn ac3_colon_id_is_skipped_and_named_in_errors() {
    let t = FakeTracker {
        cands: vec![cand("EXT:1", "Bad id", &[]), cand("EXT-2", "Fine", &[])],
        sidecars: HashMap::new(),
        fail_list_open: false,
    };
    let doc = snapshot::build(&t, "fake").unwrap();
    let entries = doc.get("entries").and_then(Value::as_array).unwrap();
    let ids: Vec<_> = entries
        .iter()
        .filter_map(|e| e.get("id").and_then(Value::as_str))
        .collect();
    assert!(!ids.contains(&"EXT:1"));
    assert!(ids.contains(&"EXT-2"));
    let errors = doc.get("errors").unwrap();
    assert!(errors.to_string().contains("EXT:1"));
}

fn graph_fixture(dir: &std::path::Path, entries: Value) -> std::path::PathBuf {
    let path = dir.join("graph.json");
    std::fs::write(&path, json!({"entries": entries}).to_string()).unwrap();
    path
}

#[test]
fn ac4_graph_list_open_excludes_terminal_and_carries_ordering_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let path = graph_fixture(
        dir.path(),
        json!([
            {"id": "ab-done", "status": "done", "completed_at": "2026-01-01T00:00:00Z"},
            {"id": "ab-sup", "status": "superseded"},
            {"id": "ab-1", "status": "ready", "priority": "p0", "rank": 2.0,
             "created_at": "2026-01-02T00:00:00Z"},
            {"id": "ab-2", "status": "idea"},
        ]),
    );
    let t = graph::GraphTracker::with_path(path);
    let cands = t.list_open().unwrap();
    let ids: Vec<_> = cands.iter().map(|c| c.node.id.as_str()).collect();
    assert_eq!(ids, ["ab-1", "ab-2"]);
    let c1 = &cands[0];
    assert_eq!(c1.priority, "p0");
    assert_eq!(c1.rank, Some(2.0));
    assert_eq!(c1.created_at.as_deref(), Some("2026-01-02T00:00:00Z"));
    let c2 = &cands[1];
    assert_eq!(c2.priority, "p2");
    assert_eq!(c2.rank, None);
}

#[test]
fn ac5_github_read_maps_not_found_and_timeout() {
    let t = GitHubTracker::new(
        Some("o/r".into()),
        Box::new(FakeGh {
            view: (1, String::new(), "Could not resolve to an issue".into()),
            list_open: (0, "[]".into(), String::new()),
            list_closed: (0, "[]".into(), String::new()),
        }),
    );
    match t.read("o/r#9") {
        Err(TrackerError::NotFound(id)) => assert_eq!(id, "o/r#9"),
        other => panic!("expected NotFound, got {:?}", other),
    }
}

#[test]
fn a_gh_io_fault_surfaces_as_backend_naming_the_id() {
    struct TimeoutGh;
    impl github::GhRun for TimeoutGh {
        fn run(&self, _args: &[String]) -> Result<(i32, String, String), String> {
            Err("gh timed out: gh issue view".into())
        }
    }
    let t = GitHubTracker::new(Some("o/r".into()), Box::new(TimeoutGh));
    match t.read("o/r#9") {
        Err(TrackerError::Backend(msg)) => assert!(msg.contains("o/r#9"), "{msg}"),
        other => panic!("expected Backend, got {:?}", other),
    }
}

#[test]
fn ac2_stale_cache_serves_the_last_good_read_on_failure() {
    let dir = tempfile::tempdir().unwrap();
    let _lock = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _env = hermetic_env(dir.path());
    std::env::set_var("FNO_TRACKER_GITHUB_REPO", "owner/a");
    let good = FakeTracker {
        cands: vec![cand("E-1", "One", &[])],
        sidecars: HashMap::new(),
        fail_list_open: false,
    };
    let bad = FakeTracker {
        cands: vec![],
        sidecars: HashMap::new(),
        fail_list_open: true,
    };
    // Prime the cache with one success, then fail with stale_ok: the cached
    // entries answer with stale_since and the failure rides in errors.
    let primed = snapshot::door_snapshot(&good, "github", false);
    assert!(primed.get("error").is_none());
    let doc = snapshot::door_snapshot(&bad, "github", true);
    assert!(doc.get("error").is_none(), "stale answer expected: {doc}");
    assert_eq!(
        doc.get("entries").unwrap().as_array().unwrap().len(),
        1,
        "the primed entry answers"
    );
    let taken = doc
        .get("stale_since")
        .and_then(Value::as_str)
        .expect("stale_since present");
    assert!(!taken.is_empty());
    assert!(doc
        .get("errors")
        .unwrap()
        .to_string()
        .contains("gh issue list failed"));
    // stale_ok=false refuses the cache.
    let refused = snapshot::door_snapshot(&bad, "github", false);
    assert!(refused.get("error").is_some());
}

#[test]
fn a_scope_switch_never_serves_another_scope_cache() {
    let dir = tempfile::tempdir().unwrap();
    let _lock = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _env = hermetic_env(dir.path());
    std::env::set_var("FNO_TRACKER_GITHUB_REPO", "owner/a");
    let good = FakeTracker {
        cands: vec![cand("E-1", "One", &[])],
        sidecars: HashMap::new(),
        fail_list_open: false,
    };
    let bad = FakeTracker {
        cands: vec![],
        sidecars: HashMap::new(),
        fail_list_open: true,
    };
    snapshot::door_snapshot(&good, "github", false);
    // Switch scope to owner/b: no cache exists there, so the failure answers
    // the error, never owner/a's cached issues.
    std::env::set_var("FNO_TRACKER_GITHUB_REPO", "owner/b");
    let doc = snapshot::door_snapshot(&bad, "github", true);
    assert!(doc.get("error").is_some(), "expected error, got {doc}");
}

#[test]
fn the_sidecar_file_reader_honors_its_contract() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sidecar");
    std::fs::create_dir_all(&root).unwrap();
    // A missing file is an empty map.
    assert_eq!(sidecar::load(&root, "NOPE-1").unwrap().len(), 0);
    // Every key passes through except id; the filename is the encoded id.
    let sc = json!({"id": "o/r#1", "cwd": "/repo", "plan_path": "/p.md"});
    std::fs::write(
        root.join(format!("{}.json", crate::claims::encode_key("o/r#1"))),
        sc.to_string(),
    )
    .unwrap();
    let loaded = sidecar::load(&root, "o/r#1").unwrap();
    assert_eq!(loaded.get("cwd"), Some(&json!("/repo")));
    assert!(loaded.get("id").is_none());
    assert!(loaded.get("title").is_none());
    // A parse error is Backend naming the id, never a panic.
    std::fs::write(
        root.join(format!("{}.json", crate::claims::encode_key("BAD-1"))),
        "not json",
    )
    .unwrap();
    match sidecar::load(&root, "BAD-1") {
        Err(TrackerError::Backend(msg)) => assert!(msg.contains("BAD-1")),
        other => panic!("expected Backend, got {:?}", other),
    }
}

#[test]
fn the_graph_backend_projects_only_sidecar_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = graph_fixture(
        dir.path(),
        json!([
            {"id": "ab-1", "title": "Graph node", "status": "ready",
             "cwd": "/repo", "plan_path": "/p.md", "pr_number": 7},
        ]),
    );
    let t = graph::GraphTracker::with_path(path);
    let sc = t.sidecar("ab-1").unwrap();
    assert_eq!(sc.get("cwd"), Some(&json!("/repo")));
    assert_eq!(sc.get("plan_path"), Some(&json!("/p.md")));
    assert_eq!(sc.get("pr_number"), Some(&json!(7)));
    // Tracker-owned names never ride the projection.
    assert!(sc.get("title").is_none());
    assert!(sc.get("status").is_none());
}

#[test]
fn door_ops_answer_refusals_in_the_payload() {
    // Unknown backend: exit-0 payload error naming the available set (AC7).
    let doc = run_door(&json!({"tracker": "read", "backend": "jira", "id": "X-1"}));
    assert_eq!(
        doc.get("error"),
        Some(&json!(
            "unknown tracker backend: jira. Available: graph, github, linear"
        ))
    );
    // Unknown op.
    let doc = run_door(&json!({"tracker": "nope", "backend": "graph"}));
    assert!(doc
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("")
        .starts_with("unknown tracker op"));
    // The graph backend refuses close: no second close path.
    let doc = run_door(&json!({"tracker": "close", "backend": "graph", "id": "ab-1"}));
    assert!(doc
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("")
        .contains("fno backlog done"));
    // An id carrying the claim-key partition character is refused.
    let doc = run_door(&json!({"tracker": "read", "backend": "graph", "id": "EX:1"}));
    assert!(doc
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("")
        .contains("partition"));
}

/// A recorded Linear API: each POST is answered by the marker its query
/// document names. Pages pop in order, so a paginated list walks them.
struct FakeLinear {
    issue: (i32, String, String),
    open_pages: Mutex<Vec<String>>,
    states: (i32, String, String),
    mutation: (i32, String, String),
    sent: Arc<Mutex<Vec<String>>>,
}

impl linear::LinearHttp for FakeLinear {
    fn post(&self, body: &str) -> Result<(i32, String, String), String> {
        self.sent.lock().unwrap().push(body.to_string());
        if body.contains("issueUpdate") {
            return Ok(self.mutation.clone());
        }
        if body.contains("workflowStates") {
            return Ok(self.states.clone());
        }
        if body.contains("TeamOpenIssues") {
            let mut pages = self.open_pages.lock().unwrap();
            return match pages.is_empty() {
                true => Ok((0, String::new(), "no pages recorded".into())),
                false => Ok((0, pages.remove(0), String::new())),
            };
        }
        Ok(self.issue.clone())
    }
}

#[test]
fn a_linear_read_maps_the_node_spec_fields() {
    let issue = r#"{"data": {"issues": {"nodes": [{
        "identifier": "ENG-7", "title": "Ship it", "priority": 1,
        "createdAt": "2026-09-01T00:00:00Z",
        "description": "the issue body",
        "url": "https://linear.app/example/issue/ENG-7",
        "state": {"type": "started"},
        "parent": {"identifier": "ENG-1"},
        "estimate": {"value": 5.0},
        "blockedBy": {"nodes": [
            {"issue": {"identifier": "ENG-7"}, "relatedIssue": {"identifier": "ENG-2"}},
            {"issue": {"identifier": "ENG-3"}, "relatedIssue": {"identifier": "ENG-7"}}]}}]}}}"#;
    let t = LinearTracker::new(
        Some("key".into()),
        Some("ENG".into()),
        Box::new(FakeLinear {
            issue: (0, issue.into(), String::new()),
            open_pages: Mutex::new(vec![]),
            states: (0, "{}".into(), String::new()),
            mutation: (0, "{}".into(), String::new()),
            sent: Arc::new(Mutex::new(vec![])),
        }),
    );
    let node = t.read("ENG-7").unwrap();
    assert_eq!(node.id, "ENG-7");
    assert_eq!(node.title.as_deref(), Some("Ship it"));
    assert_eq!(node.state, State::Open);
    assert_eq!(node.parent.as_deref(), Some("ENG-1"));
    assert_eq!(node.blocked_by, ["ENG-2", "ENG-3"]);
    assert_eq!(node.details.as_deref(), Some("the issue body"));
    assert_eq!(
        node.url.as_deref(),
        Some("https://linear.app/example/issue/ENG-7")
    );
    assert_eq!(node.size.as_deref(), Some("M"));
}

#[test]
fn a_linear_read_maps_not_found() {
    let t = LinearTracker::new(
        Some("key".into()),
        Some("ENG".into()),
        Box::new(FakeLinear {
            issue: (
                0,
                r#"{"data": {"issues": {"nodes": []}}}"#.into(),
                String::new(),
            ),
            open_pages: Mutex::new(vec![]),
            states: (0, "{}".into(), String::new()),
            mutation: (0, "{}".into(), String::new()),
            sent: Arc::new(Mutex::new(vec![])),
        }),
    );
    match t.read("ENG-9") {
        Err(TrackerError::NotFound(id)) => assert_eq!(id, "ENG-9"),
        other => panic!("expected NotFound, got {:?}", other),
    }
}

#[test]
fn a_linear_list_open_maps_priority_size_and_paginates() {
    let page1 = r#"{"data": {"team": {"issues": {
        "nodes": [
            {"identifier": "ENG-1", "title": "One", "priority": 2,
             "state": {"type": "started"}, "createdAt": "2026-09-01T00:00:00Z",
             "estimate": {"value": 5.0}, "blockedBy": {"nodes": [
                {"issue": {"identifier": "ENG-1"}, "relatedIssue": {"identifier": "ENG-2"}}]}},
            {"identifier": "ENG-2", "title": "Two", "priority": 0,
             "state": {"type": "backlog"}, "createdAt": "2026-09-02T00:00:00Z",
             "estimate": {"value": 12.0}, "blockedBy": {"nodes": []}}],
        "pageInfo": {"hasNextPage": true, "endCursor": "c1"}}}}}"#;
    let page2 = r#"{"data": {"team": {"issues": {
        "nodes": [
            {"identifier": "ENG-3", "title": "Three", "priority": 4,
             "state": {"type": "completed"}, "createdAt": "2026-09-03T00:00:00Z",
             "estimate": null, "blockedBy": {"nodes": []}}],
        "pageInfo": {"hasNextPage": false, "endCursor": null}}}}}"#;
    let t = LinearTracker::new(
        Some("key".into()),
        Some("ENG".into()),
        Box::new(FakeLinear {
            issue: (0, "{}".into(), String::new()),
            open_pages: Mutex::new(vec![page1.into(), page2.into()]),
            states: (0, "{}".into(), String::new()),
            mutation: (0, "{}".into(), String::new()),
            sent: Arc::new(Mutex::new(vec![])),
        }),
    );
    let cands = t.list_open().unwrap();
    assert_eq!(cands.len(), 3);
    assert_eq!(cands[0].priority, "p1");
    assert_eq!(cands[0].node.size.as_deref(), Some("M"));
    assert_eq!(cands[0].node.blocked_by, ["ENG-2"]);
    assert_eq!(cands[1].priority, "p2");
    assert_eq!(cands[1].node.size.as_deref(), Some("L"));
    assert_eq!(cands[2].priority, "p3");
    assert_eq!(cands[2].node.size, None);
}

#[test]
fn a_linear_close_picks_the_completed_state() {
    let states = r#"{"data": {"issues": {"nodes": [{
        "id": "uuid-7",
        "team": {"workflowStates": {"nodes": [
            {"id": "s-todo", "type": "backlog"},
            {"id": "s-done", "type": "completed"}]}}}]}}}"#;
    let sent_log = Arc::new(Mutex::new(Vec::new()));
    let t = LinearTracker::new(
        Some("key".into()),
        Some("ENG".into()),
        Box::new(FakeLinear {
            issue: (0, "{}".into(), String::new()),
            open_pages: Mutex::new(vec![]),
            states: (0, states.into(), String::new()),
            mutation: (
                0,
                r#"{"data": {"issueUpdate": {"success": true}}}"#.into(),
                String::new(),
            ),
            sent: sent_log.clone(),
        }),
    );
    t.close("ENG-7").unwrap();
    let sent = sent_log.lock().unwrap();
    assert!(sent.iter().any(|b| b.contains("s-done")));
}

#[test]
fn a_linear_tracker_without_the_api_key_refuses_naming_the_env_var() {
    let t = LinearTracker::new(
        None,
        Some("ENG".into()),
        Box::new(FakeLinear {
            issue: (0, "{}".into(), String::new()),
            open_pages: Mutex::new(vec![]),
            states: (0, "{}".into(), String::new()),
            mutation: (0, "{}".into(), String::new()),
            sent: Arc::new(Mutex::new(vec![])),
        }),
    );
    match t.read("ENG-7") {
        Err(TrackerError::Refused(msg)) => {
            assert!(msg.contains("FNO_TRACKER_LINEAR_API_KEY"), "{msg}")
        }
        other => panic!("expected Refused, got {:?}", other),
    }
}
