use super::*;
use crate::state::MuxRef;
use crate::AgentStatus;

fn roster_worker(session_id: &str, pid: u32, repl_pid: u32) -> crate::claude_roster::RosterWorker {
    serde_json::from_value(json!({"sessionId": session_id, "pid": pid, "replPid": repl_pid}))
        .unwrap()
}

fn row(name: &str, status: AgentStatus) -> RegistryEntry {
    RegistryEntry {
        name: name.into(),
        status,
        harness: Some("claude".into()),
        created_at: "2026-01-01T00:00:00Z".into(),
        ..Default::default()
    }
}

/// The union: a roster session shows once under its short id, and the fno
/// row it dedups into hands it the lead and the registry join. A pid-less
/// claude row lives through the roster, a pid-less codex row through its
/// slot claim, and a dead pid drops out. Every row reads its stored token.
#[test]
fn the_census_dedups_the_roster_and_keeps_each_liveness_arm() {
    let roster = vec![roster_worker(
        "aaaa1111-0000-0000-0000-000000000000",
        100,
        101,
    )];
    let mut adopted = row("sorrel", AgentStatus::Live);
    adopted.short_id = "aaaa1111".into();
    adopted.harness_session_id = Some("aaaa1111-0000-0000-0000-000000000000".into());
    adopted.spawned_by_session = Some("1eadbeef-lead".into());
    let mut thread = row("t-x-1234-sol", AgentStatus::Idle);
    thread.harness = Some("codex".into());
    let mut pane = row("t-x-5678-glm", AgentStatus::Spawning);
    pane.pid = Some(300);
    pane.mux = Some(MuxRef {
        session: "main".into(),
        pane_id: 4,
    });
    let mut dead = row("gone", AgentStatus::Busy);
    dead.pid = Some(400);
    let exited = row("done", AgentStatus::Exited);
    let rows = vec![adopted, thread, pane, dead, exited];

    let alive = |pid: u32, _: Option<u64>| matches!(pid, 100 | 300);
    let claim = |name: &str| name == "t-x-1234-sol";
    let c = census(&roster, &rows, &alive, &claim, now_epoch_s());

    let names: Vec<&str> = c.workers.iter().map(|w| w.name.as_str()).collect();
    assert_eq!(names, ["aaaa1111", "t-x-1234-sol", "t-x-5678-glm"]);
    let shown = &c.workers[0];
    assert_eq!(
        shown.session_pid,
        Some(101),
        "cost reads the session process, not the host"
    );
    assert_eq!(shown.spawned_by.as_deref(), Some("1eadbeef-lead"));
    assert_eq!(shown.entry, Some(0));
    assert_eq!(c.workers[1].substrate, "worker");
    assert_eq!(c.workers[2].substrate, "pane");
    assert_eq!(c.workers[2].stored_status, "quiet");
    assert_eq!(c.workers[2].status_basis, Some("stale-spawning-live-pid"));
    assert!(c.live_registry_names.contains("sorrel"));
}

/// The join a lead reads: the registry handle beside the roster label, the
/// node and PR off the session map, the role label, and RSS from the
/// session process. A row with no truth answer reads `unknown`, never quiet.
#[test]
fn a_worker_row_carries_the_handle_node_role_and_session_rss() {
    let sid = "aaaa1111-0000-0000-0000-000000000000";
    let roster = vec![roster_worker(sid, 100, 101)];
    let mut adopted = row("sorrel", AgentStatus::Live);
    adopted.short_id = "aaaa1111".into();
    adopted.harness_session_id = Some(sid.into());
    adopted.role_level = Some(2);
    adopted.role_scope = Some("x-0e67".into());
    let rows = vec![adopted];
    let c = census(&roster, &rows, &|_, _| true, &|_| false, now_epoch_s());
    let mut sessions = Map::new();
    sessions.insert(
        sid.into(),
        json!({"node": "x-36fb", "basis": "graph", "pr": 3133, "pr_basis": "node"}),
    );
    let rss = BTreeMap::from([(101u32, 1295u64)]);
    let out = worker_rows(
        &c,
        &rows,
        &HashMap::new(),
        crate::truth_probe::BatchOutcome::Measured,
        &sessions,
        &rss,
    );
    let r = &out[0];
    assert_eq!(r["name"], "aaaa1111");
    assert_eq!(r["handle"], "sorrel");
    assert_eq!(r["pid"], 101);
    assert_eq!(r["rss_mb"], 1295);
    assert_eq!(r["node"], "x-36fb");
    assert_eq!(r["pr"], 3133);
    assert_eq!(r["role"], "L2 x-0e67");
    assert_eq!(r["status"], "unknown");
    assert_eq!(r["progress"], "unknown");
}

/// Newest first, deduped by agent id, the parent off the directory when the
/// record carries none, and a scope-stated empty for an absent store.
#[test]
fn the_subagent_scan_reads_first_records_inside_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir
        .path()
        .join("-repo")
        .join("parent-session")
        .join("subagents");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(
        sub.join("agent-abc.jsonl"),
        "{\"agentId\":\"abc\",\"cwd\":\"/repo\",\"gitBranch\":\"main\"}\n{}\n",
    )
    .unwrap();
    std::fs::write(sub.join("agent-torn.jsonl"), "{not json\n").unwrap();
    let now = SystemTime::now();
    let section = subagent_section(dir.path(), 600, now);
    let rows = section["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "a torn first record is skipped: {rows:?}");
    assert_eq!(rows[0]["agent_id"], "abc");
    assert_eq!(rows[0]["parent"], "parent-s");
    assert_eq!(rows[0]["verdict"], "active");
    let later = now + std::time::Duration::from_secs(SUBAGENT_SCAN_WINDOW_S + 60);
    assert_eq!(subagent_section(dir.path(), 600, later)["rows"], json!([]));
    let absent = subagent_section(&dir.path().join("missing"), 600, now);
    assert_eq!(absent["warnings"], json!([]));
}

/// Deltas within the journal-latest session; a counter that went down is a
/// server restart, reported born-and-gone, never a negative delta.
#[test]
fn pane_counters_difference_the_latest_session_and_report_resets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    let sample = |ts: &str, session: &str, panes: Value| {
        json!({"type": "mux_pane_counters", "ts": ts, "data": {"session": session, "panes": panes}})
            .to_string()
    };
    let pane = |id: i64, bytes: i64| {
        json!({"pane_id": id, "name": "w", "bytes_in": bytes, "grid_updates": 1,
               "frames_composited": 1, "frames_emitted": 1, "cpu_ns": 2_000_000})
    };
    let lines = [
        sample("2026-10-09T10:00:00Z", "other", json!([pane(9, 1)])),
        sample(
            "2026-10-09T10:00:30Z",
            "main",
            json!([pane(1, 100), pane(2, 500), pane(3, 7)]),
        ),
        sample(
            "2026-10-09T10:01:00Z",
            "main",
            json!([pane(1, 160), pane(2, 20), pane(4, 1)]),
        ),
    ];
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    let out = pane_counter_rows(&path);
    assert_eq!(out["status"], "ok");
    assert_eq!(out["session"], "main");
    assert_eq!(out["window_s"], 30.0);
    assert_eq!(out["rows"].as_array().unwrap().len(), 1);
    assert_eq!(out["rows"][0]["bytes_in"], 60);
    assert_eq!(out["born"], json!([2, 4]));
    assert_eq!(out["gone"], json!([2, 3]));
    assert_eq!(
        pane_counter_rows(&dir.path().join("none.jsonl"))["status"],
        "insufficient-samples"
    );
}
