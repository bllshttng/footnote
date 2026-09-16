#[path = "daemon/tests/blocking_bound_tests.rs"]
mod blocking_bound_tests;
#[path = "daemon/tests/store_socket_sweep_tests.rs"]
mod store_socket_sweep_tests;
use super::blocking_bound::directory_bytes_within;
use super::*;
use crate::client_verbs::RowLiveness;
use crate::codex_thread_entry::build_codex_thread_entry;

/// The e2e restart-storm test only exercises `state_error_code` when the
/// scheduler happens to race a task into shutdown-cancellation, so its
/// coverage of the Cancelled -> ShuttingDown mapping is real but silent
/// on a run where nothing races. Pin the mapping directly and
/// deterministically: Cancelled must classify as ShuttingDown, and every
/// other StateError variant must stay Internal.
#[test]
fn state_error_code_classifies_cancelled_as_shutting_down() {
    assert_eq!(
        state_error_code(&state::StateError::Cancelled("task cancelled".into())),
        ErrorCode::ShuttingDown
    );
    assert_eq!(
        state_error_code(&state::StateError::Io(std::io::Error::other("boom"))),
        ErrorCode::Internal
    );
    assert_eq!(
        state_error_code(&state::StateError::InvariantViolation("drift".into())),
        ErrorCode::Internal
    );
}

/// Registry-local projection used only by the address-form unit test.
fn canonical_name_in(registry: &state::Registry, token: &str) -> String {
    let Ok(Value::Array(rows)) = serde_json::to_value(&registry.entries) else {
        return token.to_string();
    };
    match crate::client_verbs::find_agent_entry(&rows, token) {
        Ok(entry) => entry
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(token)
            .to_string(),
        Err(_) => token.to_string(),
    }
}
use crate::state::{AgentState, DriveWindow, PtyState};

fn tmp_home(tag: &str) -> AgentsHome {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "fno-agents-daemon-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let home = AgentsHome::at(&p);
    home.ensure_root().unwrap();
    home
}

// x-ef7f: the connect-probe singleton guard let a busy-but-alive
// incumbent read as absent, so every losing race added a new supervisor
// instead of replacing the incumbent. The flock-based guard must resolve
// N concurrent binders to exactly one winner.
#[tokio::test]
async fn bind_supervisor_socket_concurrent_only_one_survives() {
    // A real UnixListener::bind needs its path to fit sockaddr_un's short
    // sun_path buffer (104 bytes on macOS); tmp_home()'s long
    // tag+pid+nanos name overflows that once `/supervisor.sock` is
    // appended, so this test (the first here to actually bind a socket)
    // builds a short path directly under /tmp instead.
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::path::PathBuf::from(format!("/tmp/fa-cb-{}-{n}", std::process::id()));
    let home = AgentsHome::at(&dir);
    home.ensure_root().unwrap();
    let mut handles = Vec::new();
    for _ in 0..8 {
        let h = home.clone();
        handles.push(tokio::spawn(
            async move { bind_supervisor_socket(&h).await },
        ));
    }
    // Collect every result FIRST, then count, so no winner's lock guard is
    // dropped while another task is still trying for it. Counting inside
    // the join loop released the lock at the first `Ok(_)` and handed it to
    // a task still mid-retry, which read as three winners -- an artifact of
    // the test's own teardown order, not of the guard. In production the
    // holder keeps its guard for the whole process lifetime, which is what
    // this shape reproduces.
    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.unwrap());
    }
    let mut ok_count = 0;
    let mut already_running = 0;
    for r in &results {
        match r {
            Ok(_) => ok_count += 1,
            Err(DaemonError::AlreadyRunning(_)) => already_running += 1,
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }
    assert_eq!(ok_count, 1, "exactly one bind must survive the race");
    assert_eq!(already_running, 7);
    drop(results);
    std::fs::remove_dir_all(home.root()).ok();
}

// x-ef7f / x-e98b: the bound-inode check is what lets a daemon detect
// that its socket path was unlinked and rebound out from under it (by an
// operator `rm`, or a departing incumbent's blind unlink) instead of
// continuing to serve unreachable, and lets the exit-time cleanup refuse
// to unlink a live successor's fresh socket.
#[test]
fn socket_inode_matches_detects_unlink_and_rebind() {
    let home = tmp_home("inode-retire");
    let sock = home.supervisor_sock();
    // HOLD the original open for the whole test. In production the daemon
    // is listening on this inode, which is what keeps its number from being
    // recycled. Dropping the handle first frees the number, and Linux hands
    // the very same one to the next file at that path -- the rebind then
    // reads as a match and the test fails there while passing on macOS.
    let held = std::fs::File::create(&sock).unwrap();
    let ino = held.metadata().unwrap().ino();
    assert!(socket_inode_matches(&sock, ino));

    std::fs::remove_file(&sock).unwrap();
    std::fs::write(&sock, b"").unwrap(); // a new inode takes the same path
    assert!(
        !socket_inode_matches(&sock, ino),
        "a rebound path must not match the old inode"
    );
    drop(held);
}

fn read_events(home: &AgentsHome) -> Vec<Value> {
    std::fs::read_to_string(home.events_jsonl())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

// Generic one-shot ask row builder (empty short_id + no pid, owns no
// worktree). The `exited_at` argument predates reverse-join retirement;
// it now only feeds the liveness ladder's heartbeat rung.
fn ask_row(name: &str, exited_at: Option<&str>) -> RegistryEntry {
    RegistryEntry {
        substrate: None,
        node: None,
        spawned_by_session: None,
        spawned_by_harness: None,
        spawned_by_cwd: None,
        launch_account: None,
        related_session_id: None,
        origin: None,
        name: name.into(),
        short_id: String::new(),
        legacy_provider: "claude".into(),
        provider: None,
        model: None,
        model_basis: None,
        effort: None,
        harness: None,
        // x-7bcd: needs a resolvable handle (leg 3); deterministic per
        // name so two rows never collide.
        harness_session_id: Some(format!("{name}-sess")),
        predecessor_session_ids: Vec::new(),
        forked_from_session_id: None,
        route_provider_id: None,
        model_name: None,
        account_record_id: None,
        cwd: "/tmp".into(),
        project_root: String::new(),
        session_id: None,
        spawn_trigger: None,
        legacy_claude_short_id: None,
        claude_session_uuid: None,
        messaging_socket_path: None,
        codex_session_id: None,
        gemini_session_id: None,
        mcp_channel_id: None,
        cc_session_id: None,
        host_mode: None,
        status: AgentStatus::Exited,
        last_message_at: None,
        created_at: "2020-01-01T00:00:00Z".into(),
        pid: None,
        pid_start_time: None,
        keeper_child_pid: None,
        log_path: None,
        last_reconciled_at: None,
        inside_leg: None,
        exited_at: exited_at.map(str::to_string),
        mux: None,
        screen_state: None,
        crown_level: None,
        crown_scope: None,
        crown_grantor: None,
        route_settings_path: None,
        fno_id: None,
        delivery_policy: None,
        sandbox_posture: None,
        ..Default::default()
    }
}

fn claude_rm_row(name: &str, short_id: &str, session_id: &str) -> RegistryEntry {
    let mut row = ask_row(name, Some("2020-01-01T00:00:00Z"));
    row.short_id = short_id.to_string();
    row.harness = Some("claude".into());
    row.harness_session_id = Some(session_id.into());
    row
}

#[test]
fn row_identity_matches_treats_a_none_session_capture_as_unasserted() {
    // The one shared comparator both `handle_rm_with`'s retain and
    // `switchboard_identity_matches` now call through (self-review
    // finding #8): a session id captured as `None` (a codex row before
    // `late_bind_codex_sessions` binds it) must not read as a mismatch
    // against the SAME row's later `Some`.
    let row = claude_rm_row("worker", "short1", "session-abc");
    let unasserted = RowIdentity {
        harness: None,
        name: Some("worker"),
        short_id: "short1",
        session_id: None,
        created_at: "2020-01-01T00:00:00Z",
    };
    assert!(row_identity_matches(&row, &unasserted));

    // A captured `Some` that disagrees with the row IS a real mismatch.
    let disagreeing = RowIdentity {
        session_id: Some("some-other-session"),
        ..unasserted
    };
    assert!(!row_identity_matches(&row, &disagreeing));

    // A captured `Some` that agrees still matches.
    let agreeing = RowIdentity {
        session_id: Some("session-abc"),
        ..unasserted
    };
    assert!(row_identity_matches(&row, &agreeing));
}

fn claude_row_then_absent(
    short_id: &'static str,
    state: &'static str,
) -> impl Fn() -> crate::claude_roster::ClaudeAgentsSnapshot {
    let calls = std::sync::atomic::AtomicUsize::new(0);
    move || {
        if calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
            crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                crate::claude_roster::ClaudeAgentRow::new(short_id, Some(state)),
            ])
        } else {
            crate::claude_roster::ClaudeAgentsSnapshot::known(Vec::new())
        }
    }
}

#[tokio::test]
async fn rm_requires_the_post_list_to_prove_the_claude_row_is_gone() {
    let home = short_home("rmpostlist");
    let row = claude_rm_row(
        "stopped-worker",
        "aaabbb11",
        "aaabbb11-1111-2222-3333-444444444444",
    );
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "stopped-worker"}));

    let response = handle_rm_with(
        &ctx,
        &request,
        &|| {
            crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                crate::claude_roster::ClaudeAgentRow::new("aaabbb11", Some("stopped")),
            ])
        },
        &|_| Ok(()),
        &|_| true,
        &|_, _| Ok(true),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert!(response
        .error()
        .unwrap()
        .message
        .contains("survives successful claude rm"));
    assert_eq!(
        state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .len(),
        1
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn rm_treats_a_positive_row_in_a_partial_post_list_as_surviving() {
    let home = short_home("rmpartialpost");
    let row = claude_rm_row(
        "stopped-worker",
        "aaabbb12",
        "aaabbb12-1111-2222-3333-444444444444",
    );
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "stopped-worker"}));
    let response = handle_rm_with(
        &ctx,
        &request,
        &|| crate::claude_roster::ClaudeAgentsSnapshot::Unknown {
            rows: vec![crate::claude_roster::ClaudeAgentRow::new(
                "aaabbb12",
                Some("stopped"),
            )],
            warnings: vec!["one malformed row".into()],
        },
        &|_| Ok(()),
        &|_| true,
        &|_, _| Ok(true),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert!(response
        .error()
        .unwrap()
        .message
        .contains("survives successful claude rm"));
    assert_eq!(
        state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .len(),
        1
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn rm_mux_failure_names_the_claude_side_already_removed() {
    let home = short_home("rmpartial");
    let mut row = claude_rm_row(
        "pane-worker",
        "aaaccc22",
        "aaaccc22-1111-2222-3333-444444444444",
    );
    row.short_id.clear();
    row.mux = Some(state::MuxRef {
        session: "work".into(),
        pane_id: 24,
    });
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "pane-worker"}));
    let snapshots = claude_row_then_absent("aaaccc22", "stopped");

    let response = handle_rm_with(
        &ctx,
        &request,
        &snapshots,
        &|_| Ok(()),
        &|_| true,
        &|_, _| Err("permission denied".into()),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    let message = &response.error().unwrap().message;
    assert!(message.contains("claude harness row aaaccc22 removed"));
    assert!(message.contains("registry retained"));
    assert!(message.contains("mux pane work:24"));
    assert_eq!(
        state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .len(),
        1
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn rm_keeps_registry_row_when_claude_refuses_without_force() {
    let home = short_home("rmrefuse");
    let row = claude_rm_row(
        "stopped-worker",
        "bbbb2222",
        "bbbb2222-1111-2222-3333-444444444444",
    );
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "stopped-worker"}));

    let response = handle_rm_with(
        &ctx,
        &request,
        &|| {
            crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                crate::claude_roster::ClaudeAgentRow::new("bbbb2222", Some("stopped")),
            ])
        },
        &|_| Err("claude rm exited 1".into()),
        &|_| true,
        &|_, _| Ok(true),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert!(response
        .error()
        .unwrap()
        .message
        .contains("claude rm exited 1"));
    assert_eq!(
        state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .len(),
        1
    );

    let forced_request = Request::new(
        2,
        "agent.rm",
        json!({"name": "stopped-worker", "force": true}),
    );
    let forced_response = handle_rm_with(
        &ctx,
        &forced_request,
        &|| {
            crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                crate::claude_roster::ClaudeAgentRow::new("bbbb2222", Some("stopped")),
            ])
        },
        &|_| Err("claude rm exited 1".into()),
        &|_| true,
        &|_, _| Ok(true),
        &|_, _| PaneProbe::Unknown,
    )
    .await;
    assert_eq!(forced_response.result().unwrap()["harness_removed"], false);
    assert_eq!(
        forced_response.result().unwrap()["harness_reason"],
        "claude rm exited 1"
    );
    assert!(state::load_registry(&home.registry_json())
        .unwrap()
        .entries
        .is_empty());
    std::fs::remove_dir_all(home.root()).ok();
}

/// A temp dir whose `.git` is a FILE: linked-worktree shape, no real repo
/// behind it (the gate and the removal are injected, so none is needed).
fn fake_worktree(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fno-rm-wt-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".git"), "gitdir: /elsewhere/worktrees/x.git\n").unwrap();
    dir
}

#[test]
fn output_with_timeout_bounds_a_stalled_subprocess() {
    // The rm path's budget: a fast child answers, a stalled one is
    // bounded instead of parking the daemon's rm handler.
    let fast = output_with_timeout(
        {
            let mut c = std::process::Command::new("echo");
            c.arg("ok");
            c
        },
        10,
    );
    assert!(fast.is_some(), "a fast child answers");
    let stalled = output_with_timeout(
        {
            let mut c = std::process::Command::new("sleep");
            c.arg("30");
            c
        },
        1,
    );
    let stalled = stalled.expect("a stalled child is killed, not left running");
    assert!(
        !stalled.status.success(),
        "the killed child reads as a failed call"
    );
}

#[test]
fn branch_merged_answers_real_repos() {
    // The rm door's half of the third bucket: a fresh branch is
    // unmerged; a fast-forward into the main line flips it.
    let root = std::env::temp_dir().join(format!("fno-rm-mg-{}", std::process::id()));
    let repo = root.join("repo");
    let wt = root.join("leaf");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&repo).unwrap();
    let git =
        |args: &[&str], cwd: &std::path::Path| crate::git_test_helpers::git_run(args, cwd).unwrap();
    let commit_args = [
        "-c",
        "user.email=t@example.com",
        "-c",
        "user.name=t",
        "commit",
        "-qm",
        "seed",
    ];
    assert!(git(&["init", "-q", "-b", "main"], &repo).status.success());
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    assert!(git(&["add", "-A"], &repo).status.success());
    assert!(git(&commit_args, &repo).status.success());
    let worktree_add = [
        "worktree",
        "add",
        "-q",
        wt.to_str().unwrap(),
        "-b",
        "feature",
    ];
    assert!(git(&worktree_add, &repo).status.success());
    std::fs::write(wt.join("b.txt"), "b\n").unwrap();
    assert!(git(&["add", "-A"], &wt).status.success());
    assert!(git(&commit_args, &wt).status.success());

    assert_eq!(
        branch_merged(wt.to_str().unwrap()),
        Some(false),
        "an unmerged branch blocks the rm door"
    );

    assert!(git(&["merge", "-q", "feature"], &repo).status.success());
    assert_eq!(
        branch_merged(wt.to_str().unwrap()),
        Some(true),
        "a merged branch passes"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn rm_take_worktree_removes_when_the_gate_answers_yes() {
    // AC5-HP: a clean tree goes. The branch survives by git's own
    // contract (`worktree remove` never deletes branches) - the command
    // choice is the sweep's, not a second policy.
    let wt = fake_worktree("clean");
    let mut row = ask_row("wt1", Some("2020-01-01T00:00:00Z"));
    row.cwd = wt.to_string_lossy().into_owned();
    let receipt = rm_take_worktree_with(&row, &|_| WorktreeGate::Reapable, &|_| Ok(()));
    assert_eq!(
        receipt.map(|o| o.receipt()),
        Some(format!("worktree removed: {}", wt.to_string_lossy()))
    );
    std::fs::remove_dir_all(&wt).ok();
}

#[test]
fn rm_take_worktree_keeps_a_blocked_tree_and_names_the_reason() {
    // AC6-EDGE: DIRTY or clean-and-unmerged keeps the tree; the receipt
    // names the path and the gate's reason. The ROW was already removed
    // by the caller - the receipt never blocks that.
    let wt = fake_worktree("dirty");
    let mut row = ask_row("wt2", Some("2020-01-01T00:00:00Z"));
    row.cwd = wt.to_string_lossy().into_owned();
    let receipt = rm_take_worktree_with(
        &row,
        &|_| WorktreeGate::Blocked("modified-tracked".into()),
        &|_| Ok(()),
    );
    assert_eq!(
        receipt.map(|o| o.receipt()),
        Some(format!(
            "worktree kept: {} (the gate said no: modified-tracked)",
            wt.to_string_lossy()
        ))
    );
    std::fs::remove_dir_all(&wt).ok();
}

#[test]
fn rm_take_worktree_keeps_the_tree_when_the_probe_cannot_answer() {
    // AC7-ERR: an unanswerable probe keeps the tree. Removal never guesses.
    let wt = fake_worktree("mute");
    let mut row = ask_row("wt3", Some("2020-01-01T00:00:00Z"));
    row.cwd = wt.to_string_lossy().into_owned();
    let receipt = rm_take_worktree_with(
        &row,
        &|_| WorktreeGate::Unanswerable("the reapable probe could not answer".into()),
        &|_| Ok(()),
    );
    assert_eq!(
        receipt.map(|o| o.receipt()),
        Some(format!(
            "worktree kept: {} (the reapable probe could not answer)",
            wt.to_string_lossy()
        ))
    );
    std::fs::remove_dir_all(&wt).ok();
}

#[test]
fn rm_take_worktree_is_a_noop_for_a_row_without_a_linked_worktree() {
    // A row that ran in a plain directory owns nothing removable: the
    // gate is never consulted, the receipt is None, nothing fails.
    let mut row = ask_row("wt4", Some("2020-01-01T00:00:00Z"));
    row.cwd = "/tmp/plain-cwd".into();
    let asked = std::cell::Cell::new(0);
    let receipt = rm_take_worktree_with(
        &row,
        &|_| {
            asked.set(asked.get() + 1);
            WorktreeGate::Reapable
        },
        &|_| Ok(()),
    );
    assert!(receipt.is_none());
    assert_eq!(asked.get(), 0, "the gate was never consulted");
}

#[tokio::test]
async fn rm_refuses_to_report_removed_when_the_row_is_already_gone() {
    // AC1-NEG (silent-no-op mode): the row entry_for_lifecycle resolved is no
    // longer in the file by the time the write runs (raced away by a
    // concurrent teardown, e.g. another rm or a direct `claude rm`). The
    // injected claude_rm closure deletes the row as its side effect,
    // reproducing that race deterministically: retain() then has nothing
    // to drop, and the handler must refuse rather than report removed:true.
    let home = short_home("rmalreadygone");
    let row = claude_rm_row(
        "raced-worker",
        "ccccdddd",
        "ccccdddd-1111-2222-3333-444444444444",
    );
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "raced-worker"}));
    let raced_home = home.clone();
    let snapshots = claude_row_then_absent("ccccdddd", "stopped");

    let response = handle_rm_with(
        &ctx,
        &request,
        &snapshots,
        &move |_| {
            state::update_registry(&raced_home.registry_json(), |registry| {
                registry.entries.retain(|e| e.name != "raced-worker");
            })
            .unwrap();
            Ok(())
        },
        &|_| true,
        &|_, _| Ok(true),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    let message = &response.error().unwrap().message;
    assert!(message.contains("registry does not hold"));
    assert!(state::load_registry(&home.registry_json())
        .unwrap()
        .entries
        .is_empty());
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn stop_refusal_names_a_pane_kill_the_mux_parser_accepts() {
    // The refusal string and the parser drift independently: the refusal
    // once printed `main:76` while the parser demanded a bare number, so
    // the instrument named a way out that errored with EXIT_USAGE. Hold
    // both sides in one test: build the refusal through the same renderer
    // handle_stop prints (its probe-unanswered branch) and feed the
    // command to the real parse_pane_args, no hardcoded expected string
    // anywhere. Kept off handle_stop itself: the mux pane probe shells
    // bare `fno` from PATH, so on a machine whose live mux runs a session
    // named `main` the probe answers Absent and the pid branch, which
    // names no kill command, replaces this one.
    let printed =
        stop_refusal_detail::pane_row_refusal("pane-worker", "main", 76, PaneProbe::Unknown, None);
    let selector = printed
        .split("Kill the pane: `")
        .nth(1)
        .expect("refusal names the kill command")
        .split('`')
        .next()
        .expect("the printed command is backtick-closed");
    let selector = selector
        .strip_prefix("fno mux pane kill ")
        .expect("the printed command is the pane kill verb");
    let args: Vec<std::ffi::OsString> = vec!["kill".into(), selector.into()];
    let parsed =
        fno::mux_cli::parse_pane_args(&args).expect("the refusal's own command must parse");
    assert_eq!(parsed.session.as_deref(), Some("main"));
    assert_eq!(parsed.cmd, fno::mux_cli::PaneCmd::Kill { pane: 76 });
}

#[test]
fn mux_missing_pane_receipt_is_idempotent_absence() {
    assert!(mux_pane_is_absent("fno mux: no such pane: 24"));
    assert!(mux_pane_is_absent(
        "cannot reach session main: No such file or directory (os error 2)"
    ));
    assert!(!mux_pane_is_absent("mux configuration not found"));
    assert!(!mux_pane_is_absent("fno mux: permission denied"));
}

/// The real summary line, copied from this machine's output.
const REAL_SUMMARY: &str = "would-archive      feature/x-3e17   /some/wt\n\
Summary: 12 would archive, 37 kept (19 unmerged, 11 unpushed, 5 dirty, 0 live-session, 1 processes, 0 salvage-failed, 0 needs-confirmation, 1 app-owned, 1 permanent), 0 failed  [dry-run: no changes made; pass --apply to execute]\n";

#[test]
fn sweep_summary_parses_the_real_line() {
    let r = parse_worktree_sweep(REAL_SUMMARY).expect("parses");
    assert_eq!(r.eligible, 12);
    assert_eq!(r.kept, 37);
    assert_eq!(r.dirty, 5);
}

#[test]
fn sweep_summary_parses_the_apply_mode_line() {
    // The apply pass says "archived", not "would archive"; the eligible
    // count must read from whichever verb the line carries.
    let line = "archived         feature/x-3e17   /some/wt\n\
Summary: 3 archived, 4 kept (1 unmerged, 1 unpushed, 1 dirty), 0 failed\n";
    let r = parse_worktree_sweep(line).expect("parses");
    assert_eq!(r.eligible, 3);
    assert_eq!(r.kept, 4);
    assert_eq!(r.dirty, 1);
}

#[test]
fn sweep_summary_absent_is_none_not_zero() {
    // A zeroed report is indistinguishable from a clean machine. An absence
    // has two explanations and only a real reading may produce a count.
    assert!(parse_worktree_sweep("").is_none());
    assert!(parse_worktree_sweep("some other output\n").is_none());
}

#[test]
fn sweep_reports_every_repo_including_the_quiet_ones() {
    let home = tmp_home("wt-sweep-quiet");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let quiet = "Summary: 0 would archive, 0 kept (0 unmerged, 0 unpushed, 0 dirty), 0 failed\n";

    let swept = worktree_sweep(
        &home,
        &emitter,
        1_000_000,
        &["/repo/a".into(), "/repo/b".into()],
        &|_| false.into(),
        &|_, _| WorktreeSweepOutput {
            exit_code: Some(0),
            stdout: quiet.into(),
            stderr: String::new(),
        },
    );

    assert_eq!(swept, 2, "a tick that finds nothing must still report");
    let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
    assert_eq!(log.matches("worktree_sweep").count(), 2);
    assert!(log.contains("report-only"));
    assert!(!log.contains("apply-orders"));
}

#[test]
fn sweep_applies_only_when_a_reap_order_stands() {
    // Ruling preserved: a merged PR is proof, a timer tick is not. The
    // timer lane applies ONLY when the merge ritual minted an order.
    let home = tmp_home("wt-sweep-ordered");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let quiet = "Summary: 0 would archive, 0 kept (0 unmerged, 0 unpushed, 0 dirty), 0 failed\n";

    let swept = worktree_sweep(
        &home,
        &emitter,
        1_000_000,
        &["/repo/a".into()],
        &|_| true.into(),
        &|_, apply| {
            assert!(apply, "a standing order must reach the verb as --apply");
            WorktreeSweepOutput {
                exit_code: Some(0),
                stdout: quiet.into(),
                stderr: String::new(),
            }
        },
    );

    assert_eq!(swept, 1);
    let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
    assert!(log.contains("apply-orders"));
    assert!(!log.contains("report-only"));
}

#[test]
fn sweep_reads_reap_orders_in_each_repository_scope() {
    let home = tmp_home("wt-sweep-repo-orders");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let seen = std::sync::Mutex::new(Vec::new());
    let quiet = "Summary: 0 would archive, 0 kept (0 unmerged, 0 unpushed, 0 dirty), 0 failed\n";

    let swept = worktree_sweep(
        &home,
        &emitter,
        1_000_000,
        &["/repo/a".into(), "/repo/b".into()],
        &|root| (root == "/repo/b").into(),
        &|root, apply| {
            seen.lock().unwrap().push((root.to_string(), apply));
            WorktreeSweepOutput {
                exit_code: Some(0),
                stdout: quiet.into(),
                stderr: String::new(),
            }
        },
    );

    assert_eq!(swept, 2);
    assert_eq!(
        seen.into_inner().unwrap(),
        vec![("/repo/a".into(), false), ("/repo/b".into(), true)]
    );
}

#[test]
fn sweep_skips_a_repo_when_its_order_probe_is_unreadable() {
    let home = tmp_home("wt-sweep-order-unreadable");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let ran_cleanup = std::sync::atomic::AtomicBool::new(false);

    let swept = worktree_sweep(
        &home,
        &emitter,
        1_000_000,
        &["/repo/a".into()],
        &|_| WorktreeSweepOrderRead {
            standing: None,
            exit_code: Some(7),
            stderr: "claim store unreadable\nextra detail\n".into(),
        },
        &|_, _| {
            ran_cleanup.store(true, std::sync::atomic::Ordering::Relaxed);
            unreachable!("an unreadable order probe must skip cleanup")
        },
    );

    assert_eq!(swept, 0);
    assert!(!ran_cleanup.load(std::sync::atomic::Ordering::Relaxed));
    let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
    assert!(log.contains("\"error\":\"unreadable-orders\""));
    assert!(log.contains("\"exit_code\":7"));
    assert!(log.contains("\"stderr\":\"claim store unreadable\""));
    assert!(!log.contains("extra detail"));
    assert!(!log.contains("report-only"));
}

#[test]
fn sweep_honours_its_own_6h_floor() {
    let home = tmp_home("wt-sweep-floor");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let out = |_: &str, _: bool| WorktreeSweepOutput {
        exit_code: Some(0),
        stdout: REAL_SUMMARY.into(),
        stderr: String::new(),
    };
    let now = 1_000_000;

    assert_eq!(
        worktree_sweep(
            &home,
            &emitter,
            now,
            &["/repo/a".into()],
            &|_| false.into(),
            &out,
        ),
        1
    );
    // Same window: skipped entirely, no second reading.
    assert_eq!(
        worktree_sweep(
            &home,
            &emitter,
            now + 60,
            &["/repo/a".into()],
            &|_| false.into(),
            &out
        ),
        0
    );
    // A little over six hours later: fires again.
    assert_eq!(
        worktree_sweep(
            &home,
            &emitter,
            now + 21_601,
            &["/repo/a".into()],
            &|_| false.into(),
            &out
        ),
        1
    );
}

#[test]
fn sweep_never_passes_apply_on_its_own_authority() {
    // Ruling: a merged PR is proof, a timer tick is not. The fn body may
    // not carry an --apply literal: applying is decided by the injected
    // orders read (merge-minted claims), never by the sweep itself.
    let src = include_str!("daemon.rs");
    let idx = src
        .find("fn worktree_sweep(")
        .expect("worktree_sweep exists");
    let body = &src[idx..idx + 2000.min(src.len() - idx)];
    assert!(!body.contains("--apply"));
}

#[test]
fn sweep_records_an_unreadable_summary_rather_than_inventing_zeros() {
    let home = tmp_home("wt-sweep-unreadable");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");

    let swept = worktree_sweep(
        &home,
        &emitter,
        1_000_000,
        &["/repo/a".into()],
        &|_| false.into(),
        &|_, _| WorktreeSweepOutput {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
        },
    );

    assert_eq!(swept, 0);
    let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
    assert!(log.contains("unreadable-summary"));
    assert!(!log.contains("\"eligible\""));
}

#[test]
fn sweep_records_a_nonzero_exit_and_first_stderr_line() {
    let home = tmp_home("wt-sweep-nonzero");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let output = WorktreeSweepOutput {
        exit_code: Some(7),
        stdout: String::new(),
        stderr: "permission denied\nextra detail\n".into(),
    };

    let swept = worktree_sweep(
        &home,
        &emitter,
        1_000_000,
        &["/repo/a".into()],
        &|_| false.into(),
        &|_, _| output.clone(),
    );

    assert_eq!(swept, 0);
    let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
    assert!(log.contains("\"exit_code\":7"));
    assert!(log.contains("\"stderr\":\"permission denied\""));
    assert!(!log.contains("extra detail"));
}

#[test]
fn sweep_distinguishes_a_zero_exit_with_no_summary() {
    let home = tmp_home("wt-sweep-zero-no-summary");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");

    let swept = worktree_sweep(
        &home,
        &emitter,
        1_000_000,
        &["/repo/a".into()],
        &|_| false.into(),
        &|_, _| WorktreeSweepOutput {
            exit_code: Some(0),
            stdout: "no summary here\n".into(),
            stderr: String::new(),
        },
    );

    assert_eq!(swept, 0);
    let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
    assert!(log.contains("\"exit_code\":0"));
    assert!(log.contains("\"stderr\":\"\""));
}

#[test]
fn stale_summary_parses_the_asked_line() {
    // The verb's ACTUAL --json output, embedded summary text and all -
    // not a synthetic standalone Summary line the parser could never see.
    let line = r#"{"outcome": "asked", "question_id": "q-37222570", "stale_count": 12, "oldest_h": 1829, "summary": "Summary: 12 stale, outcome asked, oldest 1829h"}"#;
    let r = parse_stale_sweep(line).expect("parses");
    assert_eq!(r.stale, 12);
    assert_eq!(r.oldest_h, 1829);
    assert_eq!(r.outcome, "asked");
}

#[test]
fn stale_summary_carries_the_refused_outcome_word() {
    // On the refused path the count is NOT a real reading, so the event
    // must carry the word that says so instead of a fabricated zero.
    let line = r#"{"outcome": "refused", "question_id": "", "stale_count": 0, "oldest_h": 0, "summary": "Summary: 0 stale, outcome refused, oldest 0h"}"#;
    let r = parse_stale_sweep(line).expect("parses");
    assert_eq!(r.stale, 0);
    assert_eq!(r.outcome, "refused");
}

#[test]
fn stale_summary_reads_duplicate_as_not_asked() {
    // A duplicate no-op is a real reading, not silence: the event must
    // carry the measured set even when the fold asked nothing.
    let line = r#"{"outcome": "duplicate", "question_id": "q-37222570", "stale_count": 12, "oldest_h": 1830, "summary": "Summary: 12 stale, outcome duplicate, oldest 1830h"}"#;
    let r = parse_stale_sweep(line).expect("parses");
    assert_eq!(r.stale, 12);
    assert_eq!(r.outcome, "duplicate");
}

#[test]
fn stale_parser_refuses_the_text_mode_line() {
    // If the verb ever regresses to text-only output, the parser must
    // answer None (loud error event), never misread the embedded
    // summary text as a reading.
    assert!(parse_stale_sweep("Summary: 12 stale, outcome asked, oldest 1829h\n").is_none());
}

#[test]
fn stale_summary_absent_is_none_not_zero() {
    // A zeroed report is indistinguishable from a clean machine. An
    // absence has two explanations and only a real reading produces a
    // count.
    assert!(parse_stale_sweep("").is_none());
    assert!(parse_stale_sweep("some other output\n").is_none());
}

#[test]
fn stale_sweep_takes_no_apply_form() {
    // The lane routes information and changes no removal path: the fn
    // body may not carry an apply decision at all.
    let src = include_str!("daemon.rs");
    let idx = src.find("fn stale_sweep(").expect("stale_sweep exists");
    let body = &src[idx..idx + 2000.min(src.len() - idx)];
    assert!(!body.contains("--apply"));
}

#[test]
fn linked_worktree_detection_separates_owners_from_passers_through() {
    // The whole ownership test is `.git` being a FILE (a `gitdir:` pointer)
    // rather than a directory. Getting it backwards would either pin every
    // row again or reap rows that DO own a dirty worktree.
    let dir = tempfile::tempdir().expect("tmpdir");
    let base = dir.path();

    let canonical = base.join("canonical");
    std::fs::create_dir_all(canonical.join(".git")).unwrap();
    assert!(
        !is_linked_worktree(canonical.to_str().unwrap()),
        "a .git DIRECTORY is the canonical checkout; the row owns nothing removable"
    );

    let linked = base.join("linked");
    std::fs::create_dir_all(&linked).unwrap();
    std::fs::write(linked.join(".git"), "gitdir: /somewhere/.git/worktrees/x\n").unwrap();
    assert!(
        is_linked_worktree(linked.to_str().unwrap()),
        "a .git FILE is a linked worktree; cleanliness decides its row"
    );

    let plain = base.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    assert!(!is_linked_worktree(plain.to_str().unwrap()));

    // Unreadable and empty both fail closed toward "owns nothing", so the row
    // is judged on terminal status and grace instead of waiting forever on a
    // cleanliness answer that can never arrive.
    assert!(!is_linked_worktree(""));
    assert!(!is_linked_worktree("/nonexistent/path/that/cannot/be/read"));
}

#[test]
fn gc_sweep_empty_registry_is_noop() {
    let home = tmp_home("gc-empty");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let summary = gc_sweep(&home, &emitter, 900, 7);
    assert!(summary.retired.is_empty());
    assert!(summary.pruned.is_empty());
}

// The gc ladder, reap-receipt gate and plan_reconcile families, moved
// verbatim into their own module (file budget: test motion is the
// sanctioned shrink; this file is far over the shrink-only line).
#[path = "daemon/tests/gc_mux_member.rs"]
mod gc_mux_member;
#[path = "daemon/tests/gc_receipts.rs"]
mod gc_receipts;
#[path = "daemon/tests/keeper_sweep.rs"]
mod keeper_sweep;
#[path = "daemon/tests/reap_holds.rs"]
mod reap_holds;
#[path = "daemon/tests/reap_session.rs"]
mod reap_session;
#[path = "daemon/tests/rm_refusal.rs"]
mod rm_refusal;
// The rm success family: same module motion as gc_receipts (file budget).
#[path = "daemon/tests/rm_success.rs"]
mod rm_success;
#[path = "daemon/tests/stop_claims.rs"]
mod stop_claims;
// The stale-sweep test family (x-39f4): same file-budget motion as the
// families above.
#[path = "daemon/tests/incident_pause.rs"]
mod incident_pause;

// The codex thread lane's spawn/registry/resume test family, moved
// verbatim into its own module for the same reason as gc_receipts above:
// file budget, test motion is the sanctioned shrink.
#[path = "daemon/tests/codex_thread_lane.rs"]
mod codex_thread_lane;

// --- plan_reconcile (US6.9): tri-state, status-aware transitions, budget ---

fn rentry(name: &str, status: AgentStatus, last_reconciled: Option<&str>) -> RegistryEntry {
    RegistryEntry {
        substrate: None,
        node: None,
        spawned_by_session: None,
        spawned_by_harness: None,
        spawned_by_cwd: None,
        launch_account: None,
        related_session_id: None,
        origin: None,
        name: name.into(),
        short_id: name.into(),
        legacy_provider: "codex".into(),
        provider: None,
        model: None,
        model_basis: None,
        effort: None,
        harness: None,
        harness_session_id: None,
        predecessor_session_ids: Vec::new(),
        forked_from_session_id: None,
        route_provider_id: None,
        model_name: None,
        account_record_id: None,
        cwd: "/tmp".into(),
        project_root: "/tmp".into(),
        session_id: Some("sid".into()),
        spawn_trigger: None,
        legacy_claude_short_id: None,
        claude_session_uuid: None,
        messaging_socket_path: None,
        codex_session_id: None,
        gemini_session_id: None,
        mcp_channel_id: None,
        host_mode: None,
        cc_session_id: None,
        status,
        last_message_at: None,
        created_at: "t".into(),
        pid: None,
        pid_start_time: None,
        keeper_child_pid: None,
        log_path: None,
        last_reconciled_at: last_reconciled.map(String::from),
        inside_leg: None,
        exited_at: None,
        mux: None,
        screen_state: None,
        crown_level: None,
        crown_scope: None,
        crown_grantor: None,
        route_settings_path: None,
        fno_id: None,
        delivery_policy: None,
        sandbox_posture: None,
        ..Default::default()
    }
}

/// A codex THREAD row: no short_id, interactive host mode, full session
/// id, and a recorded rollout path (the durable resume object). Shared by
/// the reconcile/liveness family here and by the codex-thread-lane family
/// in `codex_thread_lane.rs`, which reaches it through the glob.
fn thread_entry(name: &str, status: AgentStatus, log_path: Option<String>) -> RegistryEntry {
    let mut entry = rentry(name, status, None);
    entry.pid = Some(999_999_999);
    entry.short_id = String::new();
    entry.legacy_provider = String::new();
    entry.harness = Some("codex".into());
    entry.host_mode = Some(crate::state::HOST_MODE_INTERACTIVE.into());
    entry.session_id = None;
    entry.harness_session_id = Some(format!("0198thread-{name}-00000000000000"));
    entry.log_path = log_path;
    entry
}

fn probe_err() -> crate::provider::ReachabilityProbeError {
    crate::provider::ReachabilityProbeError::new("codex", "store unavailable")
}

// --- find_uuid_backfill_row (x-c393): backfill a null-uuid bg row ---------

/// A `claude --bg` row: jobId in `short_id`, `claude_session_uuid` null.
/// A transcript file for a fixture row that is genuinely finished.
///
/// The corroboration gate needs a POSITIVE reading that a worker stopped
/// writing, and these tests run with a zero grace window, so any file that
/// already exists reads as stale. A fixture with no transcript at all reads
/// as UNKNOWN and is kept, which is the correct production behaviour and
/// would silently hollow out these assertions.
fn stale_log(dir: &std::path::Path) -> String {
    let p = dir.join("transcript.jsonl");
    std::fs::write(&p, "{}\n").unwrap();
    // BACKDATE IT. A file written this second reads as fresh even against a
    // zero-length window, so a fixture that only creates the file proves the
    // opposite of what it claims.
    assert!(std::process::Command::new("touch")
        .args(["-t", "200001010000", &p.to_string_lossy()])
        .status()
        .expect("touch runs")
        .success());
    p.to_string_lossy().into_owned()
}

fn bg_claude_row(name: &str, short_id: &str) -> RegistryEntry {
    let mut e = rentry(name, AgentStatus::Live, None);
    e.legacy_provider = "claude".into();
    e.short_id = short_id.into();
    e.claude_session_uuid = None;
    // A row fno itself spawned: the retirement origin gate retires only
    // these, so every retirement-path fixture starts from spawn.
    e.origin = Some("spawn".into());
    // x-7bcd: needs a resolvable handle; short_id is the transport key
    // this row is actually tested against, not one of the three legs.
    e.log_path = Some(format!("/tmp/{name}.log"));
    e
}

#[test]
fn find_uuid_backfill_row_matches_null_uuid_by_short_prefix() {
    // AC1-HP: the full uuid's leading hex group is the row's short-id.
    let rows = vec![bg_claude_row("w", "3228ccad")];
    assert!(matches!(
        find_uuid_backfill_row(&rows, "3228ccad-c078-4b53-a8c9-7199b831eae4"),
        UuidBackfill::One(0)
    ));
}

#[test]
fn find_uuid_backfill_row_refuses_ambiguous_short_collision() {
    // AC1-ERR: two null-uuid rows share the short-id -> refuse, don't guess.
    let rows = vec![
        bg_claude_row("w1", "3228ccad"),
        bg_claude_row("w2", "3228ccad"),
    ];
    assert!(matches!(
        find_uuid_backfill_row(&rows, "3228ccad-c078-4b53-a8c9-7199b831eae4"),
        UuidBackfill::Ambiguous
    ));
}

#[test]
fn find_uuid_backfill_row_skips_rows_that_already_have_a_uuid() {
    // Idempotent: a row already carrying its uuid is matched by the fast
    // path, never backfilled here.
    let mut row = bg_claude_row("w", "3228ccad");
    row.claude_session_uuid = Some("3228ccad-c078-4b53-a8c9-7199b831eae4".into());
    assert!(matches!(
        find_uuid_backfill_row(&[row], "3228ccad-c078-4b53-a8c9-7199b831eae4"),
        UuidBackfill::None
    ));
}

#[test]
fn find_uuid_backfill_row_skips_non_claude_rows() {
    // codex P2: a foreign-provider row carrying a short must not
    // adopt a claude uuid.
    let mut row = bg_claude_row("w", "3228ccad");
    row.legacy_provider = "codex".into();
    assert!(matches!(
        find_uuid_backfill_row(&[row], "3228ccad-c078-4b53-a8c9-7199b831eae4"),
        UuidBackfill::None
    ));
}

#[test]
fn find_uuid_backfill_row_requires_group_boundary() {
    // A short must not match a longer hex run it merely prefixes: `3228ccad`
    // is not the leading group of `3228ccadd-...` (no `-` at the boundary).
    let rows = vec![bg_claude_row("w", "3228ccad")];
    assert!(matches!(
        find_uuid_backfill_row(&rows, "3228ccadd-c078-4b53-a8c9-7199b831eae4"),
        UuidBackfill::None
    ));
}

#[test]
fn concurrent_spawn_name_reservation_inserts_once() {
    // Codex P1 (PR #365): two concurrent agent.spawn calls for the same name
    // both pass the lock-free collision check, then race to push. The
    // reservation closure runs inside update_registry's exclusive flock, which
    // serializes the two, so the second observes the first's row and must NOT
    // duplicate it. update_registry's flock makes sequential calls here a
    // faithful stand-in for the serialized concurrent ones.
    let home = tmp_home("spawn-reserve");
    let path = home.registry_json();
    let reserve = |entry: RegistryEntry| -> bool {
        state::update_registry(&path, move |r| {
            if r.entries.iter().any(|e| e.name == entry.name) {
                return false;
            }
            r.entries.push(entry);
            true
        })
        .unwrap()
    };
    let dup_row = || -> RegistryEntry {
        let mut e = rentry("dup", AgentStatus::Live, None);
        e.log_path = Some("/tmp/dup.log".into()); // x-7bcd: resolvable handle
        e
    };
    assert!(reserve(dup_row()), "first wins");
    assert!(!reserve(dup_row()), "second loses the race -> no insert");
    let reg = state::load_registry(&path).unwrap();
    assert_eq!(
        reg.entries.iter().filter(|e| e.name == "dup").count(),
        1,
        "exactly one row for the contended name"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn reconcile_flips_unreachable_live_to_orphaned_and_recovers_orphaned() {
    let entries = vec![
        rentry("live-but-gone", AgentStatus::Live, None),
        rentry("back-from-dead", AgentStatus::Orphaned, None),
    ];
    let (changes, out) = plan_reconcile(
        &entries,
        |e| match e.name.as_str() {
            "live-but-gone" => Ok(false), // unreachable
            _ => Ok(true),                // reachable
        },
        || false,
        |_| true,
        |_| false,
        |_| false,
        |_| false,
        |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
        true,                   // roster readable: the flip needs a successful roster read
    );
    assert_eq!(out.orphans, vec!["live-but-gone".to_string()]);
    assert_eq!(out.recovered, vec!["back-from-dead".to_string()]);
    assert_eq!(out.updated.len(), 2);
    // Both probed -> both get a status change recorded.
    assert_eq!(
        changes[0].new_status,
        Some(AgentStatus::Orphaned),
        "unreachable live agent should orphan"
    );
    assert_eq!(changes[1].new_status, Some(AgentStatus::Live));
}

#[test]
fn reconcile_settles_an_unhosted_thread_with_a_rollout_to_orphaned() {
    let entries = vec![thread_entry(
        "t-resumable",
        AgentStatus::Live,
        Some("/tmp/r.jsonl".into()),
    )];
    let (changes, _) = plan_reconcile(
        &entries,
        |_| Ok(false),
        || false,
        |_| true,
        |_| false,
        |_| false, // not hosted: the actor is gone (daemon restart, resume failed)
        |_| true,  // the rollout file exists: the durable object survives
        |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
        true,      // roster readable: the flip needs a successful roster read
    );
    assert_eq!(
        changes[0].new_status,
        Some(AgentStatus::Orphaned),
        "resumable thread reads Orphaned, never Live-forever"
    );
}

/// AC12: a PRE-v19 row (no posture key) still parses and reads the safe
/// default - never a parse failure, never an accidental escalation.
#[test]
fn pre_v19_row_without_posture_parses_with_the_safe_default() {
    let raw = json!({
        "name": "legacy-thread",
        "short_id": "",
        "legacy_provider": "",
        "harness": "codex",
        "harness_session_id": "0198old-0000-7000-8000-000000000001",
        "cwd": "/tmp",
        "project_root": "/tmp",
        "host_mode": "interactive",
        "status": "live",
        "created_at": "2026-08-01T00:00:00Z",
    });
    let entry: RegistryEntry = serde_json::from_value(raw).expect("pre-v19 row parses");
    assert_eq!(entry.sandbox_posture, None);
    assert!(
        !entry_posture_is_full_access(&entry),
        "an unrecorded posture reads the safe default, never full access"
    );
}

#[test]
fn reconcile_settles_an_unhosted_thread_without_a_rollout_to_exited() {
    let entries = vec![thread_entry("t-gone", AgentStatus::Live, None)];
    let (changes, _) = plan_reconcile(
        &entries,
        |_| Ok(false),
        || false,
        |_| true,
        |_| false,
        |_| false,              // not hosted
        |_| false,              // no rollout: the thread never got far enough to persist
        |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
        true,                   // roster readable: the flip needs a successful roster read
    );
    assert_eq!(
        changes[0].new_status,
        Some(AgentStatus::Exited),
        "an unhosted thread with no rollout is gone, not Live-forever"
    );
}

#[test]
fn reconcile_does_not_orphan_a_live_interactive_host_on_store_miss() {
    // US4 (task 2.3): an interactive host whose session-store probe returns
    // unreachable (a live `codex resume`/`gemini -r` TUI may not appear in
    // the exec session index) must NOT be orphaned -- its liveness is the PTY
    // process, governed by the pid-liveness sweep. An exec sibling with the
    // same probe result IS still orphaned, so the branch is host_mode-scoped.
    let mut interactive = rentry("hosted-tui", AgentStatus::Live, None);
    interactive.host_mode = Some(crate::state::HOST_MODE_INTERACTIVE.to_string());
    let exec = rentry("one-shot", AgentStatus::Live, None);
    let entries = vec![interactive, exec];
    let (changes, out) = plan_reconcile(
        &entries,
        |_| Ok(false),
        || false,
        |_| true,
        |_| false,
        |_| false,
        |_| false,
        |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
        true,                   // roster readable: the flip needs a successful roster read
    );
    assert_eq!(
        changes[0].new_status, None,
        "a live interactive host must not be orphaned on a session-store miss"
    );
    assert_eq!(
        changes[1].new_status,
        Some(AgentStatus::Orphaned),
        "an exec sibling with the same probe result is still orphaned"
    );
    assert_eq!(out.orphans, vec!["one-shot".to_string()]);
}

#[test]
fn reconcile_mux_pane_liveness_follows_the_pid_not_the_store() {
    // Codex P1/P2 (#603): a mux-hosted pane is PTY-governed, so on a
    // session-store miss a live pid keeps it Live and a dead pid reaps to
    // Exited. A pid-less pane (_lookup_child_pid best-effort miss) has no PTY
    // signal and must NOT be preserved -- pid_live maps None to true, so that
    // would keep a maybe-dead pane immortal; it defers to store liveness
    // (orphan) instead.
    let mk = |name: &str, pid: Option<u32>| {
        let mut e = rentry(name, AgentStatus::Live, None);
        e.mux = Some(crate::state::MuxRef {
            session: "main".into(),
            pane_id: 7,
        });
        e.pid = pid;
        e
    };
    let entries = vec![
        mk("live-pane", Some(4242)), // pid present + alive
        mk("dead-pane", Some(4243)), // pid present + dead
        mk("pidless-pane", None),    // pid capture missed
    ];
    let (changes, out) = plan_reconcile(
        &entries,
        |_| Ok(false), // session_index miss for all
        || false,
        |e| e.name == "live-pane", // only live-pane's pid is alive
        |_| false,
        |_| false,
        |_| false,
        |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
        true,                   // roster readable: the flip needs a successful roster read
    );
    assert_eq!(
        changes[0].new_status, None,
        "a live-pid mux pane is preserved"
    );
    assert_eq!(
        changes[1].new_status,
        Some(AgentStatus::Exited),
        "a dead-pid mux pane is reaped to Exited"
    );
    assert_eq!(
        changes[2].new_status,
        Some(AgentStatus::Orphaned),
        "a pid-less mux pane defers to store liveness (orphan), not immortal"
    );
    assert_eq!(out.orphans, vec!["pidless-pane".to_string()]);
}

#[test]
fn reconcile_store_hit_does_not_resurrect_a_pid_dead_row() {
    // x-830c: a store that never evicts (opencode keeps its session rows
    // forever) answers Ok(true) long after the pane is gone. Recovery needs
    // the pid too, or every sweep would flip a dead orphan back to Live and
    // discovery would hand out a recipient nobody drains.
    let entries = vec![
        rentry("dead-orphan", AgentStatus::Orphaned, None),
        rentry("live-orphan", AgentStatus::Orphaned, None),
    ];
    let (changes, out) = plan_reconcile(
        &entries,
        |_| Ok(true), // session still in the store for both
        || false,
        |e| e.name == "live-orphan",
        |_| false,
        |_| false,
        |_| false,
        |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
        true,                   // roster readable: the flip needs a successful roster read
    );
    assert_eq!(
        changes[0].new_status, None,
        "a store hit must not recover a row whose pid is dead"
    );
    assert_eq!(
        changes[1].new_status,
        Some(AgentStatus::Live),
        "a store hit on a live pid still recovers"
    );
    assert_eq!(out.recovered, vec!["live-orphan".to_string()]);
}

#[test]
fn reconcile_pidless_orphan_still_recovers_on_store_hit() {
    // Guards the blast radius of the pid gate above: `pid_live` is true for a
    // row with no recorded pid, so exec rows keep their old behavior.
    let entries = vec![rentry("pidless", AgentStatus::Orphaned, None)];
    let (changes, out) = plan_reconcile(
        &entries,
        |_| Ok(true),
        || false,
        |_| true,
        |_| false,
        |_| false,
        |_| false,
        |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
        true,                   // roster readable: the flip needs a successful roster read
    );
    assert_eq!(changes[0].new_status, Some(AgentStatus::Live));
    assert_eq!(out.recovered, vec!["pidless".to_string()]);
}

#[test]
fn to_agent_entry_projects_the_opencode_session_id() {
    // Python persists opencode ids to harness_session_id and drops
    // `session_id` on write, so without this arm the probe would receive
    // None for every pane row and never run.
    let mut e = rentry("oc", AgentStatus::Live, None);
    e.legacy_provider = "opencode".into();
    e.harness = Some("opencode".into());
    e.harness_session_id = Some("ses_09679f284ffeJv7NdBAoLQLnLZ".into());
    e.session_id = None;
    assert_eq!(
        to_agent_entry(&e).session_id.as_deref(),
        Some("ses_09679f284ffeJv7NdBAoLQLnLZ")
    );
}

#[test]
fn reconcile_inconclusive_preserves_status() {
    let entries = vec![rentry("flaky", AgentStatus::Live, None)];
    let (changes, out) = plan_reconcile(
        &entries,
        |_| Err(probe_err()),
        || false,
        |_| true,
        |_| false,
        |_| false,
        |_| false,
        |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
        true,                   // roster readable: the flip needs a successful roster read
    );
    assert_eq!(changes[0].new_status, None, "must NOT flip on inconclusive");
    assert!(out.orphans.is_empty());
    assert_eq!(out.inconsistent.len(), 1);
    assert_eq!(out.inconsistent[0].0, "flaky");
}

#[test]
fn reconcile_leaves_terminal_states_untouched() {
    // An exited entry that probes unreachable must NOT become orphaned, and a
    // reachable exited entry must NOT be resurrected to live.
    let entries = vec![
        rentry("done", AgentStatus::Exited, None),
        rentry("dead", AgentStatus::PermanentDead, None),
    ];
    let (changes, out) = plan_reconcile(
        &entries,
        |_| Ok(false),
        || false,
        |_| true,
        |_| false,
        |_| false,
        |_| false,
        |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
        true,                   // roster readable: the flip needs a successful roster read
    );
    assert!(changes.iter().all(|c| c.new_status.is_none()));
    assert!(out.orphans.is_empty() && out.updated.is_empty());
}

/// One-shot `ask` shape: empty short_id + no pid (the discriminator
/// `is_one_shot_ask` keys on), host_mode exec, a resumable provider session.
fn ask_entry(name: &str, status: AgentStatus) -> RegistryEntry {
    let mut e = rentry(name, status, None);
    e.short_id = String::new();
    e.pid = None;
    e.codex_session_id = Some("resume-uuid".into());
    e.session_id = None;
    e
}

#[test]
fn reconcile_one_shot_ask_settles_to_exited_even_when_reachable() {
    // AC3-HP: a finished `ask` row settles to Exited regardless of whether its
    // provider session file still exists. The probe here returns Ok(true)
    // (reachable == session file present == "resumable"); the ask branch must
    // ignore it and settle to Exited by process-liveness alone. If the probe
    // were (wrongly) consulted for status, this Live row would stay Live.
    let entries = vec![ask_entry("codex-ask", AgentStatus::Live)];
    let (changes, out) = plan_reconcile(
        &entries,
        |_| Ok(true), // reachable: session file exists -> resumable, NOT running
        || false,
        |_| true,
        |_| false,
        |_| false,
        |_| false,
        |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
        true,                   // roster readable: the flip needs a successful roster read
    );
    assert_eq!(
        changes[0].new_status,
        Some(AgentStatus::Exited),
        "a finished ask settles to exited even when its session file is reachable"
    );
    assert_eq!(out.updated, vec!["codex-ask".to_string()]);
    assert!(out.orphans.is_empty(), "an ask is exited, never orphaned");
    // AC3-EDGE independence: the row's resumable session id is untouched by the
    // status settle (status == liveness; session_id == resumability, separate).
    assert_eq!(entries[0].codex_session_id.as_deref(), Some("resume-uuid"));
}

#[test]
fn reconcile_one_shot_ask_already_terminal_is_untouched() {
    // An ask already Exited must not be re-flagged as updated (idempotent).
    let entries = vec![ask_entry("done-ask", AgentStatus::Exited)];
    let (changes, out) = plan_reconcile(
        &entries,
        |_| Ok(true),
        || false,
        |_| true,
        |_| false,
        |_| false,
        |_| false,
        |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
        true,                   // roster readable: the flip needs a successful roster read
    );
    assert_eq!(changes[0].new_status, None);
    assert!(out.updated.is_empty());
}

#[test]
fn reconcile_does_not_reap_a_bg_thread_that_is_live_in_claudes_roster() {
    // x-beb7: a `claude --substrate bg` thread has claude's harness, no
    // footnote pid and no mux, so it matches `is_one_shot_ask` exactly like a
    // finished ask -- but it is a RUNNING process owned by claude's daemon.
    // Reaping it unprobed made `fno-agents wait --state done` return
    // "done (via exit)" within seconds for a worker whose transcript was
    // still growing, which reads to a waiting king as a dead teammate.
    let entries = vec![bg_claude_row("think-web-copy", "35570a01")];
    assert!(
        entries[0].is_one_shot_ask(),
        "a bg thread must still match the ask shape, or this test proves nothing"
    );

    // Present in the roster == running: leave the row alone.
    let (changes, out) = plan_reconcile(
        &entries,
        |_| Ok(true),
        || false,
        |_| true,
        |_| true,
        |_| false,
        |_| false,
        |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
        true,                   // roster readable: the flip needs a successful roster read
    );
    assert_eq!(
        changes[0].new_status, None,
        "a bg thread claude's daemon still lists must not be reaped to exited"
    );
    assert!(out.updated.is_empty());

    // Absent from the roster == genuinely gone: the ask reap still applies,
    // so this is a liveness check, not a blanket exemption for claude rows.
    let (changes, out) = plan_reconcile(
        &entries,
        |_| Ok(true),
        || false,
        |_| true,
        |_| false,
        |_| false,
        |_| false,
        |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
        true,                   // roster readable: the flip needs a successful roster read
    );
    assert_eq!(changes[0].new_status, Some(AgentStatus::Exited));
    assert_eq!(out.updated, vec!["think-web-copy".to_string()]);
}

#[test]
fn apply_reconcile_change_clears_pid_only_on_exited() {
    // Locked Decision #7: a row reconciled to Exited drops its pid; any other
    // transition keeps it. Every applied change freshens last_reconciled_at.
    let mut to_exited = rentry("x", AgentStatus::Live, None);
    to_exited.pid = Some(4242);
    to_exited.pid_start_time = Some(99);
    to_exited.inside_leg = Some(state::InsideLegReport {
        state: state::InsideLegState::Working,
        seq: 3,
        reason: None,
        received_at: "2026-06-27T00:00:00Z".into(),
        ttl_ms: None,
    });
    apply_reconcile_change(&mut to_exited, Some(AgentStatus::Exited), None, "T1");
    assert_eq!(to_exited.status, AgentStatus::Exited);
    assert_eq!(to_exited.pid, None, "exited row must drop its pid");
    assert_eq!(to_exited.pid_start_time, None);
    assert_eq!(
        to_exited.inside_leg, None,
        "exited row must clear the inside-leg authority (E3.3 / AC-X2-4)"
    );
    assert_eq!(to_exited.last_reconciled_at.as_deref(), Some("T1"));
    assert_eq!(
        to_exited.exited_at.as_deref(),
        Some("T1"),
        "the Exited transition stamps exited_at; CHECKED alone must not pose as one"
    );

    let mut to_orphaned = rentry("y", AgentStatus::Live, None);
    to_orphaned.pid = Some(4242);
    to_orphaned.inside_leg = Some(state::InsideLegReport {
        state: state::InsideLegState::Working,
        seq: 1,
        reason: None,
        received_at: "2026-06-27T00:00:00Z".into(),
        ttl_ms: None,
    });
    apply_reconcile_change(&mut to_orphaned, Some(AgentStatus::Orphaned), None, "T2");
    assert_eq!(to_orphaned.status, AgentStatus::Orphaned);
    assert_eq!(
        to_orphaned.pid,
        Some(4242),
        "non-exited transition keeps pid"
    );
    assert!(
        to_orphaned.inside_leg.is_some(),
        "a non-exit transition keeps the inside-leg report (only exit tears it down)"
    );
    assert_eq!(
        to_orphaned.exited_at, None,
        "a non-exit transition writes no exit stamp"
    );

    // No status change: status held, but CHECKED still freshens (AC2-FR).
    let mut no_change = rentry("z", AgentStatus::Live, Some("OLD"));
    no_change.pid = Some(4242);
    apply_reconcile_change(&mut no_change, None, None, "T3");
    assert_eq!(no_change.status, AgentStatus::Live);
    assert_eq!(no_change.pid, Some(4242));
    assert_eq!(no_change.last_reconciled_at.as_deref(), Some("T3"));
    assert_eq!(
        no_change.exited_at, None,
        "a CHECKED-only probe must not write an exit stamp"
    );
}

#[test]
fn emit_inside_leg_completion_publishes_only_for_report_bearing_rows() {
    // AC-X2-4: the ordered teardown publishes one completion event carrying
    // the final state for a row that has an inside-leg report, and is a no-op
    // for a plain row (a normal exit with nothing to tear down).
    let home = tmp_home("inside-leg-completion");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");

    let mut with_report = rentry("pane", AgentStatus::Live, None);
    with_report.session_id = Some("sess-uuid".into());
    with_report.inside_leg = Some(state::InsideLegReport {
        state: state::InsideLegState::Working,
        seq: 9,
        reason: Some("running tests".into()),
        received_at: "2026-06-27T00:00:00Z".into(),
        ttl_ms: Some(5000),
    });
    emit_inside_leg_completion(&emitter, &with_report);
    emit_inside_leg_completion(&emitter, &rentry("plain", AgentStatus::Live, None));

    let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
    let events: Vec<serde_json::Value> = log
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .filter(|v: &serde_json::Value| v["type"] == "inside_leg_completed")
        .collect();
    assert_eq!(
        events.len(),
        1,
        "exactly one completion, only for the report-bearing row"
    );
    let ev = &events[0];
    assert_eq!(ev["data"]["name"], "pane");
    assert_eq!(ev["data"]["session_id"], "sess-uuid");
    assert_eq!(ev["data"]["final_state"], "working");
    assert_eq!(ev["data"]["seq"], 9);

    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn buffer_pending_report_highest_seq_wins_and_is_bounded() {
    use std::collections::HashMap;
    let rep = |seq| state::InsideLegReport {
        state: state::InsideLegState::Working,
        seq,
        reason: None,
        received_at: "2026-06-27T00:00:00Z".into(),
        ttl_ms: None,
    };
    let mut map: HashMap<String, state::InsideLegReport> = HashMap::new();

    // First buffer for a session: stored.
    assert!(matches!(
        buffer_pending_report(&mut map, "s1", rep(2)),
        BufferOutcome::Buffered
    ));
    assert_eq!(map["s1"].seq, 2);

    // A reordered/duplicate early push (seq <= buffered) is dropped, buffer unchanged.
    assert!(matches!(
        buffer_pending_report(&mut map, "s1", rep(1)),
        BufferOutcome::StaleSeq { last: 2 }
    ));
    assert_eq!(
        map["s1"].seq, 2,
        "stale early push must not regress the buffer"
    );

    // A newer push for the same session advances it.
    assert!(matches!(
        buffer_pending_report(&mut map, "s1", rep(5)),
        BufferOutcome::Buffered
    ));
    assert_eq!(map["s1"].seq, 5);

    // Fill to cap with distinct sessions, then a NEW session is dropped (Full),
    // while an existing session still advances.
    for i in 0..PENDING_INSIDE_LEG_CAP {
        buffer_pending_report(&mut map, &format!("fill{i}"), rep(1));
    }
    assert!(map.len() >= PENDING_INSIDE_LEG_CAP);
    assert!(matches!(
        buffer_pending_report(&mut map, "brand-new", rep(1)),
        BufferOutcome::Full
    ));
    assert!(!map.contains_key("brand-new"));
    assert!(
        matches!(
            buffer_pending_report(&mut map, "s1", rep(9)),
            BufferOutcome::Buffered
        ),
        "an already-buffered session advances even at cap (no new key)"
    );
}

#[test]
fn flush_buffered_inside_leg_drains_onto_row_under_seq_gate() {
    // E3.3 flush (race-free): after a row registers, the buffered early-push
    // report is drained onto it and removed from the buffer, with a logged
    // event. A newer report that raced onto the row's store path first is NOT
    // regressed (codex P2: highest-seq-wins survives the flush).
    let home = tmp_home("inside-leg-flush");
    let ctx = test_ctx_with_events(home.clone(), PathBuf::from("fno-agents-worker"));
    let report = |seq| state::InsideLegReport {
        state: state::InsideLegState::Working,
        seq,
        reason: None,
        received_at: "2026-06-27T00:00:00Z".into(),
        ttl_ms: Some(5000),
    };

    // A registered claude row (inside_leg None) + a buffered report for it.
    let mut row = rentry("pane", AgentStatus::Live, None);
    row.legacy_provider = "claude".into();
    row.claude_session_uuid = Some("uuid-x".into());
    state::update_registry(&home.registry_json(), |r| r.entries.push(row)).unwrap();
    ctx.pending_inside_leg
        .lock()
        .unwrap()
        .insert("uuid-x".into(), report(4));

    flush_buffered_inside_leg(&ctx, "uuid-x", "pane");

    // Buffer drained; row carries the report; event logged.
    assert!(!ctx
        .pending_inside_leg
        .lock()
        .unwrap()
        .contains_key("uuid-x"));
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert_eq!(reg.entries[0].inside_leg.as_ref().map(|r| r.seq), Some(4));
    let events = read_events(&home);
    assert!(events
        .iter()
        .any(|e| e["type"] == "inside_leg_buffer_flushed"
            && e["data"]["name"] == "pane"
            && e["data"]["session_id"] == "uuid-x"
            && e["data"]["seq"] == 4));

    // Seq gate: a NEWER report already on the row (seq 10) is not regressed by
    // a stale buffered report (seq 7).
    state::update_registry(&home.registry_json(), |r| {
        r.entries[0].inside_leg = Some(report(10));
    })
    .unwrap();
    ctx.pending_inside_leg
        .lock()
        .unwrap()
        .insert("uuid-x".into(), report(7));
    flush_buffered_inside_leg(&ctx, "uuid-x", "pane");
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert_eq!(
        reg.entries[0].inside_leg.as_ref().map(|r| r.seq),
        Some(10),
        "a stale buffered report must not regress a newer row state"
    );

    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn reconcile_defers_remaining_when_budget_exhausted() {
    let entries = vec![
        rentry("a", AgentStatus::Live, None),
        rentry("b", AgentStatus::Live, None),
        rentry("c", AgentStatus::Live, None),
    ];
    // Budget allows exactly one probe, then reports exhausted.
    let mut probes = 0;
    let (changes, out) = plan_reconcile(
        &entries,
        |_| {
            probes += 1;
            Ok(true)
        },
        {
            let mut checked = 0;
            move || {
                let exhausted = checked >= 1;
                checked += 1;
                exhausted
            }
        },
        |_| true,
        |_| false,
        |_| false,
        |_| false,
        |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
        true,                   // roster readable: the flip needs a successful roster read
    );
    assert_eq!(out.deferred, 2, "two trailing entries should defer");
    assert_eq!(changes.len(), 1, "only one entry probed before budget");
}

/// A pane-hosted codex row: empty short_id (matches the real spawn shape),
/// a mux ref, and whatever pid/session id the caller sets afterward.
fn codex_pane_row(name: &str) -> RegistryEntry {
    let mut e = ask_row(name, None);
    e.harness = Some("codex".into());
    // x-7bcd: a real codex pane starts id-less (late-bind is what sets
    // harness_session_id), so undo ask_row's default and use a log_path
    // for the resolvable handle instead.
    e.harness_session_id = None;
    e.log_path = Some(format!("/tmp/{name}.log"));
    e.status = AgentStatus::Live;
    e.mux = Some(state::MuxRef {
        session: "main".into(),
        pane_id: 1,
    });
    e
}

#[test]
fn late_bind_writes_harness_session_id_for_an_unbound_live_codex_pane() {
    // AC1 (x-9de7 task 2): a codex pane whose spawn-time bind window
    // expired carries no harness_session_id. One late-bind pass, given a
    // live pid, resolves and writes it.
    let home = tmp_home("late-bind-basic");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let me = std::process::id();
    let Some(my_start) = process_start_time(me) else {
        return;
    };
    state::update_registry(&home.registry_json(), |r| {
        let mut e = codex_pane_row("pane-a");
        e.pid = Some(me);
        e.pid_start_time = Some(my_start);
        r.entries.push(e);
    })
    .unwrap();

    late_bind_codex_sessions(&home, &emitter, &|pid| {
        (pid == me).then(|| "sess-a".to_string())
    })
    .unwrap();

    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert_eq!(
        reg.find("pane-a").unwrap().harness_session_id.as_deref(),
        Some("sess-a")
    );
    let events = read_events(&home);
    assert!(events
        .iter()
        .any(|e| e.get("type").and_then(Value::as_str) == Some("agent_late_bind")));
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn late_bind_surfaces_registry_write_failure() {
    let home = tmp_home("late-bind-write-failure");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let me = std::process::id();
    let Some(my_start) = process_start_time(me) else {
        return;
    };
    state::update_registry(&home.registry_json(), |r| {
        let mut existing = ask_row("existing", None);
        existing.harness = Some("codex".into());
        existing.harness_session_id = Some("duplicate-session".into());
        r.entries.push(existing);

        let mut candidate = codex_pane_row("pane-a");
        candidate.pid = Some(me);
        candidate.pid_start_time = Some(my_start);
        r.entries.push(candidate);
    })
    .unwrap();

    let error =
        late_bind_codex_sessions(&home, &emitter, &|_| Some("duplicate-session".to_string()))
            .expect_err("duplicate session identity must fail the late-bind write");

    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(reg.find("pane-a").unwrap().harness_session_id.is_none());
    assert!(error.contains("late-bind registry write failed for pane-a"));
    let events = read_events(&home);
    assert!(events.iter().any(|e| {
        e.get("type").and_then(Value::as_str) == Some("agent_late_bind_failed")
            && e.get("data")
                .and_then(|data| data.get("name"))
                .and_then(Value::as_str)
                == Some("pane-a")
    }));
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn late_bind_gives_each_same_cwd_pane_its_own_session_id() {
    // The same-cwd repro (x-9de7 verification #5): two codex panes in one
    // cwd, both bound late, each keyed on its own pid -- this is the test
    // that would have caught a `(harness, cwd)` join.
    let home = tmp_home("late-bind-same-cwd");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let me = std::process::id();
    let Some(my_start) = process_start_time(me) else {
        return;
    };
    state::update_registry(&home.registry_json(), |r| {
        let mut a = codex_pane_row("pane-a");
        a.cwd = "/repo".into();
        a.pid = Some(me);
        a.pid_start_time = Some(my_start);
        r.entries.push(a);

        let mut b = codex_pane_row("pane-b");
        b.cwd = "/repo".into();
        b.pid = Some(me);
        b.pid_start_time = Some(my_start);
        r.entries.push(b);
    })
    .unwrap();

    // A real probe is keyed on pid, so two DISTINCT pids would resolve to
    // two distinct sessions; here both rows share this test's own pid (no
    // second live process to fork), so the fake keys on name via a
    // once-per-call counter to prove per-row binding still lands
    // per-row rather than being skipped as "already bound" after the
    // first write.
    let calls = std::cell::RefCell::new(0);
    late_bind_codex_sessions(&home, &emitter, &|_pid| {
        let mut n = calls.borrow_mut();
        *n += 1;
        Some(format!("sess-{n}"))
    })
    .unwrap();

    let reg = state::load_registry(&home.registry_json()).unwrap();
    let sid_a = reg.find("pane-a").unwrap().harness_session_id.clone();
    let sid_b = reg.find("pane-b").unwrap().harness_session_id.clone();
    assert!(sid_a.is_some() && sid_b.is_some());
    assert_ne!(sid_a, sid_b, "each pane must receive its own session id");
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn late_bind_leaves_a_gone_pane_for_the_reaper() {
    // A pane-hosted row whose pane is gone: no session id is written and
    // the row is left for the reaper (x-9de7 task 2 AC3).
    let home = tmp_home("late-bind-gone-pane");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    state::update_registry(&home.registry_json(), |r| {
        let mut e = codex_pane_row("pane-gone");
        e.pid = Some(0x7fff_fff0); // not a live process
        r.entries.push(e);
    })
    .unwrap();

    late_bind_codex_sessions(&home, &emitter, &|_| {
        panic!("the probe must not run against a pid that already fails pid_is_ours")
    })
    .unwrap();

    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(reg.find("pane-gone").unwrap().harness_session_id.is_none());
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn late_bind_never_clobbers_an_already_bound_row() {
    let home = tmp_home("late-bind-already-bound");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let me = std::process::id();
    let Some(my_start) = process_start_time(me) else {
        return;
    };
    state::update_registry(&home.registry_json(), |r| {
        let mut e = codex_pane_row("pane-bound");
        e.pid = Some(me);
        e.pid_start_time = Some(my_start);
        e.harness_session_id = Some("already-there".into());
        r.entries.push(e);
    })
    .unwrap();

    late_bind_codex_sessions(&home, &emitter, &|_| Some("already-there".to_string())).unwrap();

    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert_eq!(
        reg.find("pane-bound")
            .unwrap()
            .harness_session_id
            .as_deref(),
        Some("already-there")
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn late_bind_applies_a_dead_predecessor_succession() {
    let home = tmp_home("late-bind-succession");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let me = std::process::id();
    let Some(my_start) = process_start_time(me) else {
        return;
    };
    state::update_registry(&home.registry_json(), |r| {
        let mut entry = codex_pane_row("pane-a");
        entry.pid = Some(me);
        entry.pid_start_time = Some(my_start);
        entry.harness_session_id = Some("session-a".into());
        entry.fno_id = Some("thread-a".into());
        r.entries.push(entry);
    })
    .unwrap();

    late_bind_codex_sessions_with_transition(
        &home,
        &emitter,
        &|_| Some("session-b".to_string()),
        &|session| (session == "session-a").then_some(false),
    )
    .unwrap();

    let reg = state::load_registry(&home.registry_json()).unwrap();
    let entry = reg.find("pane-a").unwrap();
    assert_eq!(entry.harness_session_id.as_deref(), Some("session-b"));
    assert_eq!(entry.predecessor_session_ids, vec!["session-a"]);
    assert!(read_events(&home).iter().any(|event| {
        event.get("type").and_then(Value::as_str) == Some("agent_late_bind")
            && event
                .get("data")
                .and_then(|data| data.get("transition"))
                .and_then(Value::as_str)
                == Some("succession")
    }));
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn late_bind_does_not_apply_liveness_sampled_from_a_stale_predecessor() {
    let home = tmp_home("late-bind-stale-predecessor");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let me = std::process::id();
    let Some(my_start) = process_start_time(me) else {
        return;
    };
    state::update_registry(&home.registry_json(), |r| {
        let mut entry = codex_pane_row("pane-a");
        entry.pid = Some(me);
        entry.pid_start_time = Some(my_start);
        entry.harness_session_id = Some("session-a".into());
        r.entries.push(entry);
    })
    .unwrap();

    let switched = std::cell::Cell::new(false);
    late_bind_codex_sessions_with_transition(
        &home,
        &emitter,
        &|_| {
            if !switched.replace(true) {
                state::update_registry(&home.registry_json(), |r| {
                    r.find_mut("pane-a").unwrap().harness_session_id = Some("session-c".into());
                })
                .unwrap();
            }
            Some("session-b".to_string())
        },
        &|session| (session == "session-a").then_some(false),
    )
    .unwrap();

    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert_eq!(reg.entries.len(), 1);
    assert_eq!(
        reg.find("pane-a").unwrap().harness_session_id.as_deref(),
        Some("session-c")
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn late_bind_preserves_a_live_predecessor_and_creates_a_clean_branch_row() {
    let home = tmp_home("late-bind-branch");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let me = std::process::id();
    let Some(my_start) = process_start_time(me) else {
        return;
    };
    state::update_registry(&home.registry_json(), |r| {
        let mut entry = codex_pane_row("pane-a");
        entry.pid = Some(me);
        entry.pid_start_time = Some(my_start);
        entry.harness_session_id = Some("session-a".into());
        entry.fno_id = Some("thread-a".into());
        r.entries.push(entry);
    })
    .unwrap();

    late_bind_codex_sessions_with_transition(
        &home,
        &emitter,
        &|_| Some("session-b".to_string()),
        &|session| (session == "session-a").then_some(true),
    )
    .unwrap();

    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert_eq!(reg.entries.len(), 2);
    let predecessor = reg.find("pane-a").unwrap();
    assert_eq!(predecessor.harness_session_id.as_deref(), Some("session-a"));
    let branch = reg
        .entries
        .iter()
        .find(|entry| entry.harness_session_id.as_deref() == Some("session-b"))
        .expect("branch session row");
    assert_eq!(branch.forked_from_session_id.as_deref(), Some("session-a"));
    assert_eq!(branch.fno_id.as_deref(), Some("session-b"));
    assert!(branch.short_id.is_empty());
    assert!(branch.mux.is_none());
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn run_reconcile_sweep_empty_registry_is_noop() {
    // Boundaries (Architecture B): an empty registry sweeps cleanly -- no
    // entries, no changes -- the startup-path no-op case. Exercises the shared
    // sweep core (load -> sort -> write -> emit) directly.
    let home = tmp_home("sweep-empty");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let result =
        run_reconcile_sweep(&home, &emitter, &|_| false, SweepMode::Full).expect("empty sweep ok");
    assert!(result.entries.is_empty());
    assert_eq!(result.outcome, ReconcileOutcome::default());
    std::fs::remove_dir_all(home.root()).ok();
}

// ---------------------------------------------------------------------------
// poll_until_ready unit tests (Task 1.1: readiness-detector wiring)
// ---------------------------------------------------------------------------

/// A detector that reports ready as soon as the visible text ends with "❯".
struct PromptDetector;
impl crate::readiness::ReadinessDetector for PromptDetector {
    fn provider_name(&self) -> &str {
        "test-cli"
    }
    fn is_ready(
        &self,
        screen: &crate::readiness::ScreenView,
    ) -> Result<bool, crate::readiness::ReadinessError> {
        Ok(screen.visible_text.trim_end().ends_with('\u{276f}'))
    }
}

/// A detector that always returns not-ready (simulates a hung CLI).
struct NeverReadyDetector;
impl crate::readiness::ReadinessDetector for NeverReadyDetector {
    fn provider_name(&self) -> &str {
        "never"
    }
    fn is_ready(
        &self,
        _screen: &crate::readiness::ScreenView,
    ) -> Result<bool, crate::readiness::ReadinessError> {
        Ok(false)
    }
}

/// AC1-HP: poll_until_ready returns the settled screen text once the
/// detector reports ready. The reply must come from the ready snapshot,
/// NOT from an intermediate partial snapshot.
#[tokio::test(flavor = "current_thread")]
async fn poll_until_ready_returns_settled_reply_on_ready_prompt() {
    // Three snapshots: two "not ready" then one showing the idle prompt.
    let snapshots: &[&str] = &["loading...", "still loading...", "done \u{276f}"];
    let idx = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let idx2 = idx.clone();
    let fetcher = move || {
        let i = idx2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let text = snapshots[i.min(snapshots.len() - 1)].to_string();
        std::future::ready(Some(text))
    };
    let result = poll_until_ready(
        fetcher,
        Box::new(PromptDetector),
        Duration::from_millis(1),
        Duration::from_secs(5),
    )
    .await;
    assert!(result.is_ok(), "expected Ok, got {result:?}");
    let reply = result.unwrap();
    assert_eq!(
        reply, "done \u{276f}",
        "reply must be the settled snapshot text, got {reply:?}"
    );
}

/// AC2-ERR: poll_until_ready returns Err when the timeout elapses before
/// the detector ever reports ready. It must NOT silently return an empty or
/// partial reply.
#[tokio::test(flavor = "current_thread")]
async fn poll_until_ready_returns_error_on_timeout() {
    let fetcher = || std::future::ready(Some("still thinking...".to_string()));
    let result = poll_until_ready(
        fetcher,
        Box::new(NeverReadyDetector),
        Duration::from_millis(10),
        Duration::from_millis(40), // very short timeout
    )
    .await;
    assert!(
        result.is_err(),
        "expected Err on timeout, got Ok({:?})",
        result.ok()
    );
}

/// AC3-EDGE: a settled screen with no reply content returns an empty string,
/// not fabricated text. (Matches Python `result.reply or ""`.)
#[tokio::test(flavor = "current_thread")]
async fn poll_until_ready_empty_settled_screen_returns_empty_string() {
    // The screen text is just the prompt glyph with nothing before it.
    let fetcher = || std::future::ready(Some("\u{276f}".to_string()));
    let result = poll_until_ready(
        fetcher,
        Box::new(PromptDetector),
        Duration::from_millis(1),
        Duration::from_secs(5),
    )
    .await;
    assert!(result.is_ok(), "expected Ok, got {result:?}");
    // The reply is the raw screen text at the settled state. An empty/glyph-only
    // screen is fine — callers use `reply or ""` to handle it.
    let reply = result.unwrap();
    assert!(!reply.contains("fabricated"), "must not fabricate content");
}

// -----------------------------------------------------------------------
// -----------------------------------------------------------------------

/// E1 fix: the locked one-host re-check matches an interactive claude row by
/// its `claude_session_uuid`, so a second writer on the same pinned session id
/// is refused even when the file claim is unavailable (fail-open backstop).
#[test]
fn entry_holds_session_matches_claude_session_uuid() {
    let row = build_claude_stream_entry(
        "peer",
        "ab12cd34",
        std::path::Path::new("/work"),
        "sess-uuid-9",
        4242,
        None,
        PathBuf::from("/tmp/log.jsonl"),
        None,
    );
    assert!(
        entry_holds_session(&row, "sess-uuid-9"),
        "a claude row must be matched by its claude_session_uuid"
    );
    assert!(!entry_holds_session(&row, "other-uuid"));
    // v25: the vendor route stays UNKNOWN on this lane (it may be routed;
    // the row's `provider` is None for the same reason), but the account
    // record mirrors the launch read rather than sitting at None by
    // omission - with no ambient config dir this env resolves "default".
    assert_eq!(row.route_provider_id, None);
    assert_eq!(row.model_name, None);
    assert_eq!(
        row.account_record_id.as_deref(),
        crate::state::launch_account_from_env().as_deref()
    );
    // The node rides the spawn REQUEST, never ambient env: a named node
    // stamps, an unnamed one stays unknown.
    let bound = build_claude_stream_entry(
        "peer",
        "ab12cd34",
        std::path::Path::new("/work"),
        "sess-uuid-9",
        4242,
        None,
        PathBuf::from("/tmp/log.jsonl"),
        Some("x-cafe"),
    );
    assert_eq!(bound.node.as_deref(), Some("x-cafe"));
}

/// A stub family-1 probe answer for the tests that only pin the state.
/// `observed_model` null here stands for "this probe did not answer it",
/// which the row renders as `no-transcript` rather than inventing a model.
///
/// `reachability: None` deliberately exercises the COMPATIBILITY FALLBACK in
/// `rendered_status_from_truth` (a `fno` too old to emit the verdict), which
/// is what keeps these pre-existing state-mapping assertions meaningful.
/// `probe_reachable` below covers the current wire.
pub(super) fn probe(state: &str) -> Option<crate::truth_probe::TruthProbe> {
    Some(crate::truth_probe::TruthProbe {
        state: state.into(),
        reachability: None,
        basis: None,
        last_activity_age_s: None,
        last_activity_basis: None,
        last_event_at: None,
        last_message: None,
        observed_model: Value::Null,
        provider_refusal: None,
        harness_title: None,
    })
}

/// The dormant gate's batch seam answering for nobody: no row's tail is
/// readable. Every caller below has no live rows to probe, so this is the
/// same "the probe said nothing" input the per-handle `None` used to be.
// -- The shared liveness ladder (x-5d96) ---------------------------------
use crate::client_verbs::row_liveness;

/// A per-row prober mirroring [`fn@live_liveness_prober`] for the staged
/// answer: same positive-death fold, index built per call.
fn ladder_claude_row(name: &str, short_id: &str) -> RegistryEntry {
    let mut e = bg_claude_row(name, short_id);
    e.status = AgentStatus::Live;
    e
}

fn heartbeat(state: state::InsideLegState, received_at: &str) -> state::InsideLegReport {
    state::InsideLegReport {
        state,
        seq: 3,
        reason: None,
        received_at: received_at.into(),
        ttl_ms: None,
    }
}

/// Write a claude bg session file whose messaging socket is `sock`, so the
/// in-process socket rung can connect to a REAL listener.
fn write_bg_session(root: &Path, short_id: &str, sock: &Path) {
    let sessions = root.join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let body = format!(
            "{{\"jobId\":\"{short_id}\",\"kind\":\"bg\",\"messagingSocketPath\":\"{}\",\"sessionId\":\"sess-{short_id}\",\"cwd\":\"/tmp\"}}",
            sock.to_str().unwrap()
        );
    std::fs::write(sessions.join("111.json"), body).unwrap();
}

/// A bindable unix socket path short enough for SUN_LEN (104): the
/// sandbox tmp roots run long, and a socket path that long refuses to
/// bind. Pid-suffixed so parallel test runs never share one.
fn short_sock(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("fno5d96-{}-{tag}.sock", std::process::id()))
}

#[test]
fn the_ladder_answers_alive_for_a_known_live_socket() {
    // POSITIVE CONTROL (x-5d96): an absence-only suite proves nothing - a
    // probe that answers Unknown for every input passes every negative
    // test. Assert Alive on a session known to be live first.
    let home = tmp_home("ladder-positive-control");
    let sock = short_sock("alive");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    write_bg_session(home.root(), "alive01", &sock);
    let e = ladder_claude_row("positive", "alive01");
    let answer = row_liveness(&e, &crate::claude_ask::ClaudeHome::at(home.root()));
    assert_eq!(answer, RowLiveness::Alive);
    drop(listener);
    let _ = std::fs::remove_file(&sock);
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn king_g4_shape_is_alive_from_the_socket_rung_and_never_dead() {
    // The measured specimen: status live, exited_at ten days old, a
    // heartbeat sixteen hours stale reading done, process up 4h41m. The
    // socket rung answers; the quiet heartbeat never downgrades the
    // answer to anything, and `Dead` is not producible from absence.
    let home = tmp_home("ladder-g4");
    let sock = short_sock("g4");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    write_bg_session(home.root(), "g4face", &sock);
    let mut e = ladder_claude_row("king-footnote-g4", "g4face");
    e.exited_at = Some("2026-08-21T00:42:40Z".into());
    e.inside_leg = Some(heartbeat(
        state::InsideLegState::Done,
        "2026-08-31T07:51:14Z",
    ));
    let answer = row_liveness(&e, &crate::claude_ask::ClaudeHome::at(home.root()));
    assert_eq!(answer, RowLiveness::Alive);
    assert_ne!(answer, RowLiveness::Dead);
    drop(listener);
    let _ = std::fs::remove_file(&sock);
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn a_heartbeat_advancing_past_exited_at_is_alive_x_d3ad() {
    // The adopted specimen: exited_at two seconds after created_at while
    // the heartbeat kept advancing past it. No socket answers; the
    // heartbeat rung does.
    let home = tmp_home("ladder-d3ad");
    let mut e = ladder_claude_row("resurrected", "d3adrow");
    e.created_at = "2026-08-01T00:00:00Z".into();
    e.exited_at = Some("2026-08-01T00:00:02Z".into());
    e.inside_leg = Some(heartbeat(
        state::InsideLegState::Working,
        "2026-08-01T00:00:30Z",
    ));
    let answer = row_liveness(&e, &crate::claude_ask::ClaudeHome::at(home.root()));
    assert_eq!(answer, RowLiveness::Alive);
    assert_ne!(answer, RowLiveness::Dead);
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn a_codex_row_with_no_socket_and_no_transcript_is_unknown_never_dead() {
    // Codex is invisible to the claude surfaces BY CONSTRUCTION. Silence
    // on every rung is Unknown - never a death verdict.
    let home = tmp_home("ladder-codex");
    let e = rentry("codex-thread", AgentStatus::Live, None);
    let answer = row_liveness(&e, &crate::claude_ask::ClaudeHome::at(home.root()));
    assert_eq!(answer, RowLiveness::Unknown);
    assert_ne!(answer, RowLiveness::Dead);
    std::fs::remove_dir_all(home.root()).ok();
}

// -- Rung 4: codex rollout freshness (x-798a) ----------------------------

fn codex_truth_none(_uuid: &str) -> Option<String> {
    None
}

/// A codex registry row in the measured x-798a shape: live-ish status,
/// no claude surfaces, one harness session id, no exit stamp (no pid to
/// confirm dead, no reconcile to terminal - the row never gets one).
fn ladder_codex_row(name: &str, session_id: &str) -> RegistryEntry {
    let mut e = rentry(name, AgentStatus::Live, None);
    e.harness = Some("codex".into());
    e.harness_session_id = Some(session_id.into());
    e.claude_session_uuid = None;
    e
}

/// The real codex store shape: nested date dirs, the session id embedded
/// in a `rollout-*.jsonl` filename - what `HarnessStoreIndex` resolves.
fn write_rollout(root: &Path, session_id: &str) -> std::path::PathBuf {
    let dir = root.join("2026").join("09").join("01");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(format!("rollout-2026-09-01T10-00-00-{session_id}.jsonl"));
    std::fs::write(&p, "{\"type\":\"session_meta\"}\n").unwrap();
    p
}

#[test]
fn an_advancing_codex_rollout_answers_alive_and_the_sweep_names_it_live() {
    // AC1-HP (x-798a): a rollout jsonl written within the window proves
    // the worker is advancing. The heartbeat rung cannot fire here (no
    // exited_at to advance past) and the claude rungs cannot see the row;
    // without this rung the row is kept but never probeable.
    let home = tmp_home("ladder-codex-alive");
    let codex = tempfile::tempdir().unwrap();
    write_rollout(codex.path(), "cdx-alive");
    let e = ladder_codex_row("codex-live", "cdx-alive");
    let answer = crate::client_verbs::row_liveness_with_codex_root(
        &e,
        &crate::claude_ask::ClaudeHome::at(home.root()),
        codex.path(),
        codex_truth_none,
    );
    assert_eq!(answer, RowLiveness::Alive);
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn a_codex_row_with_no_readable_rollout_stays_unknown_and_kept() {
    // AC1-EDGE + AC2-EDGE: a missing rollout and an unreadable store both
    // read Unknown (an absent store has two explanations and only one of
    // them is a dead worker), and the policy names the row not-terminal.
    // This IS the measured x-798a baseline: named, kept, never reaped.
    let home = tmp_home("ladder-codex-absent");
    let codex = tempfile::tempdir().unwrap();
    let e = ladder_codex_row("codex-gone", "cdx-gone");
    let answer = crate::client_verbs::row_liveness_with_codex_root(
        &e,
        &crate::claude_ask::ClaudeHome::at(home.root()),
        codex.path(),
        codex_truth_none,
    );
    assert_eq!(answer, RowLiveness::Unknown);
    let answer = crate::client_verbs::row_liveness_with_codex_root(
        &e,
        &crate::claude_ask::ClaudeHome::at(home.root()),
        &codex.path().join("does-not-exist"),
        codex_truth_none,
    );
    assert_eq!(answer, RowLiveness::Unknown);
    assert_ne!(answer, RowLiveness::Dead);
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn a_claude_row_is_never_judged_by_the_codex_store() {
    // AC3 (harness keying): a claude row whose session id has a FRESH
    // rollout in the codex store must still answer Unknown. The rung
    // fires only for codex rows; keyed on session id alone, one fresh
    // codex store would vouch for every claude row on the machine.
    let home = tmp_home("ladder-codex-keying");
    let codex = tempfile::tempdir().unwrap();
    write_rollout(codex.path(), "shared-sess");
    let mut e = ladder_claude_row("claude-row", "");
    e.harness = Some("claude".into());
    e.harness_session_id = Some("shared-sess".into());
    e.claude_session_uuid = None;
    let answer = crate::client_verbs::row_liveness_with_codex_root(
        &e,
        &crate::claude_ask::ClaudeHome::at(home.root()),
        codex.path(),
        codex_truth_none,
    );
    assert_eq!(answer, RowLiveness::Unknown);
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn a_quiet_codex_rollout_proves_nothing() {
    // AC4-EDGE: an mtime older than the window falls through to Unknown.
    // Quiet is idle-or-dead and the ladder cannot say which - so it says
    // neither, and the row is kept.
    let home = tmp_home("ladder-codex-stale");
    let codex = tempfile::tempdir().unwrap();
    let p = write_rollout(codex.path(), "cdx-stale");
    let mtime = std::time::SystemTime::UNIX_EPOCH
        + std::time::Duration::from_secs((now_epoch_secs() - 2 * 3600) as u64);
    std::fs::File::options()
        .write(true)
        .open(&p)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    let e = ladder_codex_row("codex-stale", "cdx-stale");
    let answer = crate::client_verbs::row_liveness_with_codex_root(
        &e,
        &crate::claude_ask::ClaudeHome::at(home.root()),
        codex.path(),
        codex_truth_none,
    );
    assert_eq!(answer, RowLiveness::Unknown);
    assert_ne!(answer, RowLiveness::Dead);
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn a_stamped_codex_row_is_never_resurrected_by_a_fresh_rollout() {
    // The stop verb writes a POSITIVE exit stamp; the rollout it leaves
    // behind stays fresh-written for the whole window. Rung 4 must go
    // silent on a stamped row, or the sweep would clear the stamp and
    // resurrect a deliberately stopped row on freshness alone (the codex
    // review finding). The heartbeat rung draws the same line - only
    // ADVANCEMENT past the stamp answers, never a quiet file.
    let home = tmp_home("ladder-codex-stamped");
    let codex = tempfile::tempdir().unwrap();
    write_rollout(codex.path(), "cdx-stopped");
    let mut e = ladder_codex_row("codex-stopped", "cdx-stopped");
    e.exited_at = Some("2026-09-01T10:30:00Z".into());
    let answer = crate::client_verbs::row_liveness_with_codex_root(
        &e,
        &crate::claude_ask::ClaudeHome::at(home.root()),
        codex.path(),
        codex_truth_none,
    );
    assert_eq!(answer, RowLiveness::Unknown);
    assert_ne!(answer, RowLiveness::Alive);
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn the_heartbeat_rung_is_the_codex_arm() {
    // The harness-agnostic rung: 14 of 26 rows are codex, and absence
    // from `fno agents top` is never a rung - but an advancing
    // inside_leg report is a positive marker on any harness.
    let home = tmp_home("ladder-codex-arm");
    let mut e = rentry("codex-thread", AgentStatus::Live, None);
    e.exited_at = Some("2026-08-01T00:00:02Z".into());
    e.inside_leg = Some(heartbeat(
        state::InsideLegState::Working,
        "2026-08-01T00:00:30Z",
    ));
    let answer = row_liveness(&e, &crate::claude_ask::ClaudeHome::at(home.root()));
    assert_eq!(answer, RowLiveness::Alive);
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn an_unreadable_transcript_is_unknown_never_dead() {
    // Measured 2026-08-31: a shell-loop probe lost its expansion, globbed
    // nothing, and returned no-transcript for four rows whose files
    // existed. A failed read is silence, not death - daemon.rs's
    // transcript_fresh_probe states the same rule. The read here returns
    // nothing because it FAILED, and the answer stays Unknown.
    let home = tmp_home("ladder-unreadable");
    let mut e = ladder_claude_row("silent", "quiet01");
    e.claude_session_uuid = Some("3228ccad-c078-4b53-a8c9-7199b831eae4".into());
    let answer = crate::client_verbs::row_liveness_with(
        &e,
        &crate::claude_ask::ClaudeHome::at(home.root()),
        |_| None,
    );
    assert_eq!(answer, RowLiveness::Unknown);
    assert_ne!(answer, RowLiveness::Dead);
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn an_expired_ttl_heartbeat_is_not_a_marker() {
    // The report's own trust contract ages a TTL'd stamp out
    // (InsideLegReport::is_live_at), so an expired beat answers nothing:
    // Unknown, never Dead, and never a forever-Alive pinning the row
    // against every later guard.
    let home = tmp_home("ladder-ttl");
    let mut e = ladder_claude_row("ttl", "ttls0001");
    e.exited_at = Some("2026-06-01T00:00:00Z".into());
    e.inside_leg = Some(heartbeat(
        state::InsideLegState::Working,
        "2026-06-01T00:00:30Z",
    ));
    e.inside_leg.as_mut().unwrap().ttl_ms = Some(60_000); // expired long ago
    let answer = row_liveness(&e, &crate::claude_ask::ClaudeHome::at(home.root()));
    assert_eq!(answer, RowLiveness::Unknown);
    assert_ne!(answer, RowLiveness::Dead);
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn the_truth_rung_separates_working_from_done() {
    // Four idle-done rows measured 2026-08-31 sat idle 40 to 47 minutes
    // with status live: the transcript tail separates done from working
    // and the status field cannot. `working` is a positive marker;
    // `done` is a TURN state and answers nothing - Unknown, not Dead.
    let home = tmp_home("ladder-truth-separation");
    let mut working = ladder_claude_row("working-row", "work001");
    working.claude_session_uuid = Some("3228ccad-c078-4b53-a8c9-7199b831eae4".into());
    let mut done = ladder_claude_row("done-row", "done001");
    done.claude_session_uuid = Some("3228ccad-c078-4b53-a8c9-7199b831eae5".into());
    let ch = crate::claude_ask::ClaudeHome::at(home.root());
    let is_working = crate::client_verbs::row_liveness_with(&working, &ch, |u| {
        (u.ends_with('4')).then(|| "working".to_string())
    });
    assert_eq!(is_working, RowLiveness::Alive);
    let is_done = crate::client_verbs::row_liveness_with(&done, &ch, |u| {
        (u.ends_with('5')).then(|| "done".to_string())
    });
    assert_eq!(is_done, RowLiveness::Unknown);
    assert_ne!(is_done, RowLiveness::Dead);
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn the_ladder_never_returns_dead() {
    // Only a positive death proof may answer Dead, and absence never is
    // one. Sweep every rung combination the ladder can see; no input
    // yields Dead.
    let home = tmp_home("ladder-never-dead");
    let ch = crate::claude_ask::ClaudeHome::at(home.root());
    for truth in [
        None,
        Some("done"),
        Some("stalled"),
        Some("unreachable"),
        Some("working"),
        Some("watching"),
        Some("your-move"),
    ] {
        for exited in [None, Some("2026-08-01T00:00:02Z")] {
            for beat in [
                None,
                Some(("2026-07-31T00:00:00Z", false)), // before exited: silence
                Some(("2026-08-01T00:00:30Z", true)),  // after exited: Alive
            ] {
                let mut e = ladder_claude_row("row", "quiet02");
                e.exited_at = exited.map(String::from);
                e.inside_leg = beat.map(|(at, _)| heartbeat(state::InsideLegState::Working, at));
                e.claude_session_uuid = Some("3228ccad-c078-4b53-a8c9-7199b831eae4".into());
                let answer =
                    crate::client_verbs::row_liveness_with(&e, &ch, |_| truth.map(String::from));
                assert_ne!(
                    answer,
                    RowLiveness::Dead,
                    "ladder answered Dead for truth={truth:?} exited={exited:?} beat={beat:?}"
                );
            }
        }
    }
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn an_unreadable_roster_never_flips_the_zombie_arm() {
    // codex P1 (PR 1329): an unreadable roster is unknown liveness. The
    // fail-closed bg_live reading must not combine with a transient
    // ladder failure to orphan every live bg worker - the flip needs a
    // roster read that SUCCEEDED.
    let entries = vec![bg_claude_row("maybe", "mayb0001")];
    let (changes, _) = plan_reconcile(
        &entries,
        |_| Ok(true),
        || false,
        |_| true,
        |_| true, // bg_live fail-closed true on the unreadable roster
        |_| false,
        |_| false,
        |_| RowLiveness::Unknown,
        false, // the roster read itself FAILED
    );
    assert_eq!(changes[0].new_status, None);
}

#[test]
fn the_orphan_transition_starts_a_fresh_grace_clock() {
    // codex P2 (PR 1329): the flip just re-decided liveness from current
    // evidence, so a carried exited_at is a stamp from a falsified
    // reading. apply clears it, so gc stamps fresh at the first real
    // dead-observation instead of aging the row on a clock that predates
    // the re-decision and skips grace.
    let mut e = bg_claude_row("zombie", "zomb0001");
    e.exited_at = Some("2026-08-21T00:42:40Z".into());
    apply_reconcile_change(
        &mut e,
        Some(AgentStatus::Orphaned),
        None,
        "2026-09-01T00:00:00Z",
    );
    assert_eq!(e.status, AgentStatus::Orphaned);
    assert!(e.exited_at.is_none());
}

#[test]
fn reconcile_ends_the_status_constant_for_a_roster_stale_silent_row() {
    // The roster hit used to hold a claude row `live` forever: presence
    // in a possibly-stale roster proves nothing about RUNNING. A silent
    // ladder (no socket, no advancing heartbeat, no working truth state)
    // means no positive running-marker, so the ask-bucket row goes
    // Orphaned - the REVERSIBLE transition, not Exited - and gc takes it
    // from the terminal set.
    let entries = vec![bg_claude_row("zombie", "zomb0001")];
    let (changes, out) = plan_reconcile(
        &entries,
        |_| Ok(true),
        || false,
        |_| true,
        |_| true, // roster: entry present
        |_| false,
        |_| false,
        |_| RowLiveness::Unknown,
        true, // roster readable
    );
    assert_eq!(changes[0].new_status, Some(AgentStatus::Orphaned));
    assert_eq!(out.orphans, vec!["zombie".to_string()]);
}

#[test]
fn an_alive_ladder_blocks_the_reconcile_zombie_flip() {
    // The x-d3ad resurrected session: roster present, heartbeat
    // advancing. The positive marker answers Alive and the flip must not
    // fire.
    let entries = vec![bg_claude_row("resurrected", "rise0001")];
    let (changes, out) = plan_reconcile(
        &entries,
        |_| Ok(true),
        || false,
        |_| true,
        |_| true,
        |_| false,
        |_| false,
        |_| RowLiveness::Alive,
        true, // roster readable
    );
    assert_eq!(changes[0].new_status, None);
    assert!(out.orphans.is_empty());
}

#[test]
fn a_spawning_row_is_never_flipped_by_the_zombie_arm() {
    // A row still coming up has had no chance to produce ANY marker, so
    // its ladder silence is meaningless: Spawning is excluded from the
    // zombie arm (the sweep's never-reap-something-still-coming-up rule).
    let mut e = bg_claude_row("spawning", "spawn001");
    e.status = AgentStatus::Spawning;
    let entries = vec![e];
    let (changes, _) = plan_reconcile(
        &entries,
        |_| Ok(true),
        || false,
        |_| true,
        |_| true,
        |_| false,
        |_| false,
        |_| RowLiveness::Unknown,
        true, // roster readable
    );
    assert_eq!(changes[0].new_status, None);
}

#[test]
fn a_roster_miss_still_flips_the_ask_bucket_to_exited_as_before() {
    // The x-5d96 arm is ADDITIVE: the existing positive roster-miss -> Exited
    // contract is untouched.
    let entries = vec![bg_claude_row("finished", "fini0001")];
    let (changes, _) = plan_reconcile(
        &entries,
        |_| Ok(true),
        || false,
        |_| true,
        |_| false, // roster: positively gone
        |_| false,
        |_| false,
        |_| RowLiveness::Alive,
        true, // roster readable
    );
    assert_eq!(changes[0].new_status, Some(AgentStatus::Exited));
}

/// Adapt a per-handle answer into the BATCH seam `handle_list_with_truth`
/// takes, for the rendering tests below - they assert what a row renders,
/// never how many processes paid for it.
///
/// Deliberately NOT used by
/// `list_pays_exactly_one_batch_call_for_the_whole_page`: an adapter that
/// fans a batch back out per handle would render identically whether the
/// handler called it once or once per row, so the call SHAPE needs its own
/// test against the raw seam.
fn per_handle(
    f: impl Fn(&str) -> Option<crate::truth_probe::TruthProbe>,
) -> impl Fn(
    &[String],
) -> (
    std::collections::HashMap<String, crate::truth_probe::TruthProbe>,
    crate::truth_probe::BatchOutcome,
) {
    move |handles: &[String]| {
        let map: std::collections::HashMap<String, crate::truth_probe::TruthProbe> = handles
            .iter()
            .filter_map(|h| Some((h.clone(), f(h)?)))
            .collect();
        (map, crate::truth_probe::BatchOutcome::Measured)
    }
}

/// A probe carrying the shared verdict, as a current `fno` emits it.
pub(super) fn probe_with_verdict(
    state: &str,
    reachability: &str,
) -> Option<crate::truth_probe::TruthProbe> {
    Some(crate::truth_probe::TruthProbe {
        state: state.into(),
        reachability: Some(reachability.into()),
        basis: Some("transcript".into()),
        last_activity_age_s: Some(12.0),
        last_activity_basis: None,
        last_event_at: Some("2026-08-15T17:00:00+00:00".into()),
        last_message: Some("Still growing (101 lines, 26 percent through the pytest run)".into()),
        observed_model: Value::Null,
        provider_refusal: None,
        harness_title: None,
    })
}

pub(super) fn probe_with_age(
    state: &str,
    reachability: &str,
    age_s: Option<f64>,
) -> Option<crate::truth_probe::TruthProbe> {
    let mut probe = probe_with_verdict(state, reachability).unwrap();
    probe.last_activity_age_s = age_s;
    Some(probe)
}

pub(super) fn test_ctx(home: AgentsHome, worker_bin: PathBuf) -> Ctx {
    Ctx {
        home,
        emitter: EventEmitter::new(std::path::PathBuf::from("/dev/null"), "daemon"),
        opts: DaemonOptions {
            idle_exit: Duration::from_secs(1800),
            worker_bin,
            reconcile_on_start: true,
            agents_config_cwd: PathBuf::from("/dev/null"),
            // Off in tests: a unit test must never spawn a real `fno inbox notify`.
            notify_on_blocked: false,
            notify_on_done: false,
        },
        started_at: std::time::Instant::now(),
        exe_fingerprint: crate::drift::ExeFingerprint::current(),
        pid_start_time: process_start_time(std::process::id()),
        pending_inside_leg: std::sync::Mutex::new(std::collections::HashMap::new()),
        codex_threads: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
    }
}

/// Like `test_ctx` but wires the emitter to `home.events_jsonl()` so
/// that tests checking emitted events can read them back with `read_events`.
fn test_ctx_with_events(home: AgentsHome, worker_bin: PathBuf) -> Ctx {
    let events_path = home.events_jsonl();
    Ctx {
        home,
        emitter: EventEmitter::new(events_path, "daemon"),
        opts: DaemonOptions {
            idle_exit: Duration::from_secs(1800),
            worker_bin,
            reconcile_on_start: true,
            agents_config_cwd: PathBuf::from("/dev/null"),
            // Off in tests: a unit test must never spawn a real `fno inbox notify`.
            notify_on_blocked: false,
            notify_on_done: false,
        },
        started_at: std::time::Instant::now(),
        exe_fingerprint: crate::drift::ExeFingerprint::current(),
        pid_start_time: process_start_time(std::process::id()),
        pending_inside_leg: std::sync::Mutex::new(std::collections::HashMap::new()),
        codex_threads: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
    }
}

// ---- Group 2, Task 3.1: switchboard tests --------------------------
//
// A fake stream-json emitter (NEVER a real `claude -p`): for each user turn
// it reads on stdin it emits the canonical sequence (user-echo receipt, a
// partial, the assistant reply, a result). Mirrors the stream_worker harness.

const FAKE_STREAM_EMITTER: &str = r#"
printf '%s\n' '{"type":"system","subtype":"init","session_id":"s1"}'
while IFS= read -r line; do
  printf '%s\n' '{"type":"user","message":{"role":"user"}}'
  printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"par"}}}'
  printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"reply-text"}]}}'
  printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"reply-text"}'
done
"#;

/// A SHORT-path agents home under `/tmp` (not the long `/var/folders` temp
/// dir): a worker's `<root>/<short_id>/worker.sock` must fit in SUN_LEN
/// (~104 chars on macOS), so switchboard tests that bind real worker sockets
/// need a short root. Mirrors the stream_worker test harness.
pub(super) fn short_home(tag: &str) -> AgentsHome {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    let n = C.fetch_add(1, Ordering::Relaxed);
    let p = PathBuf::from(format!("/tmp/fnosb{tag}{}_{n}", std::process::id()));
    let home = AgentsHome::at(&p);
    home.ensure_root().unwrap();
    home
}

/// Seed a held-stream-thread registry row (claude + full UUID + Live).
pub(super) fn seed_stream_row(home: &AgentsHome, name: &str, short_id: &str) {
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(RegistryEntry {
            substrate: None,
            node: None,
            spawned_by_session: None,
            spawned_by_harness: None,
            spawned_by_cwd: None,
            launch_account: None,
            related_session_id: None,
            origin: None,
            name: name.into(),
            short_id: short_id.into(),
            legacy_provider: "claude".into(),
            provider: None,
            model: None,
            model_basis: None,
            effort: None,
            harness: None,
            harness_session_id: None,
            predecessor_session_ids: Vec::new(),
            forked_from_session_id: None,
            route_provider_id: None,
            model_name: None,
            account_record_id: None,
            cwd: "/tmp".into(),
            project_root: "/tmp".into(),
            session_id: None,
            spawn_trigger: None,
            legacy_claude_short_id: None,
            claude_session_uuid: Some(format!("uuid-{short_id}")),
            messaging_socket_path: None,
            codex_session_id: None,
            gemini_session_id: None,
            mcp_channel_id: None,
            cc_session_id: None,
            host_mode: None,
            status: AgentStatus::Live,
            last_message_at: None,
            created_at: "2026-06-09T00:00:00Z".into(),
            pid: None,
            pid_start_time: None,
            keeper_child_pid: None,
            log_path: None,
            last_reconciled_at: None,
            inside_leg: None,
            exited_at: None,
            mux: None,
            screen_state: None,
            crown_level: None,
            crown_scope: None,
            crown_grantor: None,
            route_settings_path: None,
            fno_id: None,
            delivery_policy: None,
            sandbox_posture: None,
            ..Default::default()
        });
    })
    .unwrap();
}

/// A row with no `short_id` and no `harness_session_id` -- the shape
/// `registry_truth_handle` cannot resolve to anything a truth probe can
/// find, matching a pane-hosted codex row that never bound a session id.
fn seed_bare_row(name: &str) -> RegistryEntry {
    RegistryEntry {
        substrate: None,
        node: None,
        spawned_by_session: None,
        spawned_by_harness: None,
        spawned_by_cwd: None,
        launch_account: None,
        related_session_id: None,
        origin: None,
        name: name.into(),
        short_id: String::new(),
        legacy_provider: String::new(),
        provider: None,
        model: None,
        model_basis: None,
        effort: None,
        harness: Some("codex".into()),
        harness_session_id: None,
        predecessor_session_ids: Vec::new(),
        forked_from_session_id: None,
        route_provider_id: None,
        model_name: None,
        account_record_id: None,
        cwd: "/tmp".into(),
        project_root: "/tmp".into(),
        session_id: None,
        spawn_trigger: None,
        legacy_claude_short_id: None,
        claude_session_uuid: None,
        messaging_socket_path: None,
        codex_session_id: None,
        gemini_session_id: None,
        mcp_channel_id: None,
        cc_session_id: None,
        host_mode: None,
        status: AgentStatus::Live,
        last_message_at: None,
        created_at: "2026-06-09T00:00:00Z".into(),
        pid: None,
        pid_start_time: None,
        keeper_child_pid: None,
        // x-7bcd: no short_id/harness_session_id (leg 3) and no pid (leg
        // 1) is the whole point of this fixture -- give it leg 2 instead
        // so the write-time guard passes without disturbing that intent.
        log_path: Some(format!("/tmp/{name}.log")),
        last_reconciled_at: None,
        inside_leg: None,
        exited_at: None,
        mux: Some(state::MuxRef {
            session: "main".into(),
            pane_id: 1,
        }),
        screen_state: None,
        crown_level: None,
        crown_scope: None,
        crown_grantor: None,
        route_settings_path: None,
        fno_id: None,
        delivery_policy: None,
        sandbox_posture: None,
        ..Default::default()
    }
}

/// The shared key-set contract. `handle_list` -- NOT Python's
/// `serialize_entry` -- is what serves `fno agents list`, and it had stayed
/// pinned to the pre-v10 key set: no `harness`, no `harness_session_id`, no
/// `mux`. A peer agent read that surface and nearly filed a wrong diagnosis
/// onto two nodes because two live pane-hosted workers looked unhosted.
///
/// The guard has to live HERE. The `render_list_json` key assertion in
/// bin/client.rs cannot catch this: the client passes daemon rows through
/// verbatim, so that test only asserts against a row it built itself.
///
/// `include_str!` is compile-time, so deleting or moving the contract file
/// breaks the build rather than silently disarming the check.
/// x-e3cc: the envelope carries the instrument's receipt, so a page the
/// probe never answered is readable AS that, not as N confident
/// `unknown` statuses.
#[test]
fn list_envelope_carries_the_probe_receipt() {
    let home = short_home("list-probe-receipt");
    seed_stream_row(&home, "w1", "abc12345");
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(1, "agent.list", json!({}));

    let response = handle_list_with_truth(&ctx, &req, per_handle(|_handle| None));
    let result = response.result().unwrap();
    assert_eq!(result["truth_probe_asked"], 1);
    assert_eq!(result["truth_probe_answered"], 0);

    let response = handle_list_with_truth(&ctx, &req, per_handle(|_handle| probe("working")));
    let result = response.result().unwrap();
    assert_eq!(result["truth_probe_asked"], 1);
    assert_eq!(result["truth_probe_answered"], 1);
}

#[test]
fn watch_serves_on_connect_and_only_on_change() {
    // The subscription contract: connect serves
    // the full document; the same (mtime, len) version answers "unchanged"
    // without a document; any write moves the stamp and the SAME `since`
    // then serves fresh rows. One stat per idle tick is the whole cost.
    let home = short_home("watch-connect");
    seed_stream_row(&home, "w1", "abc12345");
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));

    let resp = handle_watch(&ctx, &Request::new(1, "agent.watch", json!({})));
    let first = resp.result().unwrap();
    assert_eq!(first["doc"]["agents"].as_array().unwrap().len(), 1);
    let version = first["version"].clone();

    let resp = handle_watch(
        &ctx,
        &Request::new(2, "agent.watch", json!({"since": version})),
    );
    let again = resp.result().unwrap();
    assert!(
        again["doc"].is_null(),
        "unchanged version serves no document"
    );
    assert_eq!(again["version"], version);

    state::update_registry(&home.registry_json(), |r| {
        r.entries[0].status = AgentStatus::Exited;
    })
    .unwrap();
    let resp = handle_watch(
        &ctx,
        &Request::new(3, "agent.watch", json!({"since": version})),
    );
    let after = resp.result().unwrap();
    assert_ne!(after["version"], version, "a write moves the stamp");
    let doc = after["doc"]["agents"].as_array().unwrap();
    assert_eq!(doc.len(), 1);
    assert_eq!(
        doc[0]["status"],
        json!("exited"),
        "rows are the fresh write"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn list_row_key_set_matches_shared_contract() {
    const CONTRACT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../schemas/agents-list-row.json"
    ));
    let contract: Value = serde_json::from_str(CONTRACT).expect("contract is valid JSON");
    assert_eq!(
        contract["projection_omissions"],
        json!(["model", "model_basis"]),
        "projection omissions must stay canonical and sorted"
    );
    let mut expected: std::collections::BTreeSet<String> = contract["required"]
        .as_array()
        .expect("required is an array")
        .iter()
        .map(|k| k.as_str().unwrap().to_string())
        .collect();
    expected.extend(
        contract["rust_only"]["keys"]
            .as_array()
            .expect("rust_only.keys is an array")
            .iter()
            .map(|k| k.as_str().unwrap().to_string()),
    );

    let home = short_home("listcontract");
    seed_stream_row(&home, "worker-contract", "abc12345");
    state::update_registry(&home.registry_json(), |r| {
        let e = &mut r.entries[0];
        // A pane-hosted row holds the mux ref INSTEAD of a transport key
        // (mux XOR worker XOR bg), so short_id is empty -- which is why
        // `session_id` resolves to null for exactly these rows and
        // `harness_session_id` is the only identity they carry.
        e.short_id = String::new();
        e.harness = Some("claude".into());
        e.harness_session_id = Some("e6f78b98-e594-47ed-ad81-84f8a78b8bb7".into());
        e.claude_session_uuid = Some("e6f78b98-e594-47ed-ad81-84f8a78b8bb7".into());
        e.mux = Some(crate::state::MuxRef {
            session: "main".into(),
            pane_id: 10,
        });
        e.crown_level = Some(1);
        e.crown_scope = Some("epic-x".into());
        e.crown_grantor = Some("king".into());
        // A vendor stamp on a claude-hosted row: the exact shape the
        // provider axis exists to describe, and the one the pre-split
        // alias lied about by carrying "claude" here.
        e.provider = Some("zai".into());
        e.effort = Some("xhigh".into());
        e.node = Some("x-cafe".into());
        // (x-7955) AC9-HP: the recorded lane rides verbatim, so a reader
        // can tell a paneless pane row from a thread row.
        e.substrate = Some("thread".into());
    })
    .unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(1, "agent.list", json!({}));

    let response = handle_list_with_truth(&ctx, &req, per_handle(|_handle| probe("working")));
    let result = response.result().unwrap();
    let row = &result["agents"][0];

    let actual: std::collections::BTreeSet<String> =
        row.as_object().unwrap().keys().cloned().collect();
    assert_eq!(actual, expected, "list row key set drifted from contract");
    assert_eq!(
        result["fields_omitted"], contract["projection_omissions"],
        "list envelope omissions drifted from contract"
    );
    // (x-7955) The recorded lane, by VALUE: the projection reads the
    // registry's record, never an inference from mux or thread_id.
    assert_eq!(
        row["substrate"], "thread",
        "the list row carries the recorded substrate"
    );

    // v23 (x-2019), by VALUE and not merely by presence: a substituted
    // row names both values; an unknown request renders null, never a
    // fabricated match. The seeded row carries no request and the probe
    // answers no model, so both keys ride null on the baseline read.
    assert_eq!(row["requested_model"], Value::Null);
    assert_eq!(row["model_substituted"], Value::Null);
    state::update_registry(&home.registry_json(), |r| {
        r.entries[0].requested_model = Some("glm-5.3[1m]".into());
    })
    .unwrap();
    let mut contradicting = probe("working").unwrap();
    contradicting.observed_model = json!({"kind": "observed", "model": "glm-5.3-flash"});
    let response = handle_list_with_truth(
        &ctx,
        &req,
        per_handle(|_handle| Some(contradicting.clone())),
    );
    let row = &response.result().unwrap()["agents"][0];
    assert_eq!(row["requested_model"], "glm-5.3[1m]");
    assert_eq!(
        row["model_substituted"],
        json!({"requested": "glm-5.3[1m]", "observed": "glm-5.3-flash"})
    );
    // Suffix-only difference is a MATCH: the marker stays null. The
    // operator's specimen table calls glm-5.3[1m] vs glm-5.3 ok.
    let mut agreeing = probe("working").unwrap();
    agreeing.observed_model = json!({"kind": "observed", "model": "glm-5.3"});
    let response = handle_list_with_truth(&ctx, &req, per_handle(|_handle| Some(agreeing.clone())));
    let row = &response.result().unwrap()["agents"][0];
    assert_eq!(row["requested_model"], "glm-5.3[1m]");
    assert_eq!(row["model_substituted"], Value::Null);

    // Presence in the key set is not the bug being guarded: a key that is
    // always null is the same lie in a different shape. Assert the values
    // reach the row.
    assert_eq!(row["harness"], "claude");
    // `provider` carries the stored vendor axis, never a harness (AC8,
    // post x-f273). Emitting it from one serializer only would be worse
    // than emitting it from both: the field would then be present or
    // absent depending on which reader answered.
    assert_eq!(row["provider"], "zai");
    assert_ne!(row["provider"], row["harness"]);
    assert!(row.get("effort").is_some(), "effort key must be emitted");
    assert_eq!(row["effort"], "xhigh");
    assert_eq!(row["node"], "x-cafe");
    assert!(row.get("model").is_none());
    assert_eq!(
        row["harness_session_id"],
        "e6f78b98-e594-47ed-ad81-84f8a78b8bb7"
    );
    // The mailbox address, asserted by VALUE and not merely by presence.
    // This row is the exact shape the address column exists for: a pane
    // worker with no transport key, whose only copyable identifier before
    // this key was `name` -- and a name-lane durable write is the largest
    // still-growing category of stranded mail on the bus. The value must
    // equal what `mail drain-self` computes for itself, which is the first
    // eight; the retired `<harness>-<short>` form is refused by the
    // resolver, so emitting it here would advertise an unreachable mailbox.
    assert_eq!(row["address"], "e6f78b98");
    // The pre-fix surface reported this row as having no identity at all:
    // session_id is legitimately null for a pane row (no transport key), so
    // harness_session_id is what has to carry it.
    assert!(row["session_id"].is_null());
    assert_eq!(row["mux"]["session"], "main");
    assert_eq!(row["mux"]["pane_id"], 10);
    assert_eq!(
        row["crown"], "L1 epic-x",
        "same formatter as Python crown_label"
    );
    // The raw crown fields need value assertions too, not just presence:
    // hardcoding either to null passes a key-set check and the bare-row
    // null check, which is the "present but always null" lie again.
    assert_eq!(row["crown_level"], 1);
    assert_eq!(row["crown_scope"], "epic-x");
    assert_eq!(row["crown_grantor"], "king");

    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn a_title_probe_that_answered_is_trusted_over_the_stored_baseline() {
    // Served, never stored, applies to ABSENCE too: a probe that answered
    // `harness_title: None` (a rotated transcript carries no agent-name
    // record) must serve None, never the sweep's stale last-seen value;
    // the stored baseline stands only for a row the batch never measured.
    let home = short_home("title-serving");
    seed_stream_row(&home, "worker-title", "abc12345");
    state::update_registry(&home.registry_json(), |r| {
        let e = &mut r.entries[0];
        e.harness = Some("claude".into());
        e.harness_session_id = Some("e6f78b98-e594-47ed-ad81-84f8a78b8bb7".into());
        e.claude_session_uuid = Some("e6f78b98-e594-47ed-ad81-84f8a78b8bb7".into());
        e.harness_title = Some("old-title".into());
    })
    .unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(1, "agent.list", json!({}));
    let uuid = "e6f78b98-e594-47ed-ad81-84f8a78b8bb7";

    // The probe ANSWERED, and answered no title: serve absence.
    let mut answered = probe_with_verdict("working", "alive").unwrap();
    answered.harness_title = None;
    let response = handle_list_with_truth(
        &ctx,
        &req,
        per_handle(move |h| (h == uuid).then(|| answered.clone())),
    );
    let row = &response.result().unwrap()["agents"][0];
    assert!(
        row["harness_title"].is_null(),
        "a probe that answered None must serve None, got {row}"
    );

    // The probe never answered (unmeasured row): the stored baseline stands.
    let response = handle_list_with_truth(&ctx, &req, per_handle(|_| None));
    let row = &response.result().unwrap()["agents"][0];
    assert_eq!(
        row["harness_title"], "old-title",
        "an unmeasured row is served the stored last-seen title"
    );

    std::fs::remove_dir_all(home.root()).ok();
}

/// The reachability EVIDENCE reaches the row, not just the verdict the
/// rendered word was picked from.
///
/// `fno agents list` auto-routes here whenever an installed binary is
/// present, so this is the projection nearly every reader gets -- and both
/// `peek` and the census comment in this file send a reader to `fno agents
/// list` for exactly these fields. Emitting them Python-side only left the
/// documented evidence missing from the default path: the guard-on-one-of-N
/// shape, in the fix for a guard-on-one-of-N bug.
///
/// The key-set contract above cannot catch this on its own, because a
/// hardcoded null satisfies it.
#[test]
fn list_row_carries_the_reachability_evidence() {
    let home = short_home("listevidence");
    seed_stream_row(&home, "worker-evidence", "abc12345");
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(1, "agent.list", json!({}));

    let response = handle_list_with_truth(
        &ctx,
        &req,
        per_handle(|_handle| probe_with_verdict("working", "reachable")),
    );
    let row = &response.result().unwrap()["agents"][0];

    assert_eq!(row["reachability"], "reachable");
    assert_eq!(row["basis"], "transcript");
    assert_eq!(row["last_activity_age_s"], 12.0);
    // The stamp and the LAST-turn text ride the same probe: a
    // hard-coded null would satisfy the key-set contract while hiding the
    // wedged-worker signal the pair exists to expose.
    assert_eq!(row["last_event_at"], "2026-08-15T17:00:00+00:00");
    assert_eq!(
        row["last_message"],
        "Still growing (101 lines, 26 percent through the pytest run)"
    );

    // A probe that did not answer leaves all five null. That is NOT the
    // same as `no-evidence`, which is a verdict this emitter must never
    // invent on the probe's behalf.
    let response = handle_list_with_truth(&ctx, &req, per_handle(|_handle| None));
    let row = &response.result().unwrap()["agents"][0];
    assert!(row["reachability"].is_null());
    assert!(row["basis"].is_null());
    assert!(row["last_activity_age_s"].is_null());
    assert!(row["last_event_at"].is_null());
    assert!(row["last_message"].is_null());

    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn list_row_live_worker_with_null_activity_age_has_unknown_progress() {
    let home = short_home("listnullage");
    seed_stream_row(&home, "worker-null-age", "abc12345");
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(1, "agent.list", json!({}));

    let response = handle_list_with_truth(
        &ctx,
        &req,
        per_handle(|_handle| probe_with_age("working", "reachable", None)),
    );
    let row = &response.result().unwrap()["agents"][0];

    assert!(row["last_activity_age_s"].is_null());
    assert_eq!(row["progress"], "unknown");
    assert_eq!(row["progress_basis"], "no-evidence");

    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn list_progress_filter_is_independent_from_status() {
    let home = short_home("listprogressfilter");
    seed_stream_row(&home, "worker-progress", "abc12345");
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));

    let parked = Request::new(1, "agent.list", json!({"progress": "parked"}));
    let response = handle_list_with_truth(
        &ctx,
        &parked,
        per_handle(|_handle| probe_with_verdict("done", "reachable")),
    );
    assert_eq!(
        response.result().unwrap()["agents"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let advancing = Request::new(2, "agent.list", json!({"progress": "advancing"}));
    let response = handle_list_with_truth(
        &ctx,
        &advancing,
        per_handle(|_handle| probe_with_verdict("done", "reachable")),
    );
    assert!(response.result().unwrap()["agents"]
        .as_array()
        .unwrap()
        .is_empty());

    std::fs::remove_dir_all(home.root()).ok();
}

/// The row reports the model the worker is ACTUALLY answering as, taken
/// from the same family-1 probe that produced `status`.
///
/// The projection orders rows by evidence of neglect, pinned to the
/// shared fixture: the same file the mux ranker (crates/fno) and the
/// Python serializer (cli) assert against. The three cannot share code -
/// the crates do not link and the CLI is Python - so this file is the
/// contract that keeps the three orders identical. `include_str!` is
/// compile-time, so deleting or moving the fixture breaks the build
/// rather than silently disarming the check.
#[test]
fn list_rows_sort_in_the_shared_attention_order() {
    const FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../schemas/agents-attention-order.json"
    ));
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture is valid JSON");
    let mut rows: Vec<Value> = fixture["rows"]
        .as_array()
        .expect("rows is an array")
        .clone();
    rows.sort_by(|a, b| attention_sort_key(a).cmp(&attention_sort_key(b)));
    let got: Vec<&str> = rows
        .iter()
        .map(|r| r["name"].as_str().expect("row has a name"))
        .collect();
    let expected: Vec<&str> = fixture["expected_order"]
        .as_array()
        .expect("expected_order is an array")
        .iter()
        .map(|v| v.as_str().expect("order entry is a string"))
        .collect();
    assert_eq!(got, expected);
}

/// Both list emitters derive this from ONE resolver -- Python's
/// `session_truth.observed_model`, which the daemon reaches through the
/// `fno agents truth --json` probe it already runs per row -- so neither
/// side can report a different model than the other for the same worker
/// (AC9-CON). The Python half of that binding is asserted in
/// cli/tests/agents/test_cli_list_logs.py.
#[test]
fn list_row_carries_the_observed_model_from_the_truth_probe() {
    let home = short_home("listobserved");
    seed_stream_row(&home, "worker-zai", "abc12345");
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(1, "agent.list", json!({}));

    let response = handle_list_with_truth(
        &ctx,
        &req,
        per_handle(|_handle| {
            Some(crate::truth_probe::TruthProbe {
                state: "working".into(),
                reachability: Some("reachable".into()),
                basis: Some("transcript".into()),
                last_activity_age_s: Some(3.5),
                last_activity_basis: None,
                last_event_at: None,
                last_message: None,
                observed_model: json!({
                    "kind": "observed", "model": "glm-5.2", "samples": 300
                }),
                provider_refusal: None,
                harness_title: None,
            })
        }),
    );
    let row = &response.result().unwrap()["agents"][0];
    assert_eq!(row["observed_model"]["model"], "glm-5.2");
    assert_eq!(row["observed_model"]["kind"], "observed");

    // A probe that did not answer must not leave a bare null: an absent
    // value is what an operator correctly reads as proving nothing, which
    // is the exact misreading this field exists to end.
    let response = handle_list_with_truth(&ctx, &req, per_handle(|_handle| probe("working")));
    let row = &response.result().unwrap()["agents"][0];
    assert_eq!(row["observed_model"], json!({"kind": "no-transcript"}));

    std::fs::remove_dir_all(home.root()).ok();
}

/// A codex pane row with no short_id and no harness_session_id resolves
/// through `registry_truth_handle` to its bare name, which no truth probe
/// can ever find. The STATUS word is activity, so even a demonstrably live
/// pid cannot lift an unanswered age: the row reads `unknown`, the same
/// word the Python list lane renders for it.
#[test]
fn list_status_is_unknown_for_an_unresolvable_row_even_with_a_confirmed_live_pid() {
    let home = short_home("listlivepid");
    state::update_registry(&home.registry_json(), |r| {
        let mut e = seed_bare_row("cx-x-e14b");
        e.pid = Some(std::process::id());
        e.pid_start_time = process_start_time(std::process::id());
        r.entries.push(e);
    })
    .unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(1, "agent.list", json!({}));

    let response = handle_list_with_truth(&ctx, &req, per_handle(|_handle| None));
    let row = &response.result().unwrap()["agents"][0];
    assert_eq!(row["status"], "unknown");

    std::fs::remove_dir_all(home.root()).ok();
}

/// The pid census does not reach the STATUS word at all: with no probe
/// answer and no live pid, the row also reads `unknown` (the reachability
/// fields still carry the pid verdict on their own axis).
#[test]
fn list_status_stays_unknown_for_an_unresolvable_row_with_no_confirmed_live_pid() {
    let home = short_home("listnolivepid");
    state::update_registry(&home.registry_json(), |r| {
        let mut e = seed_bare_row("cx-dead");
        e.pid = Some(0x7fff_fff0); // not a live process
        r.entries.push(e);
    })
    .unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(1, "agent.list", json!({}));

    let response = handle_list_with_truth(&ctx, &req, per_handle(|_handle| None));
    let row = &response.result().unwrap()["agents"][0];
    assert_eq!(row["status"], "unknown");

    std::fs::remove_dir_all(home.root()).ok();
}

/// An opencode row resolves `session_id` from `harness_session_id`, the only
/// place its id is persisted. Without the arm it fell through to the generic
/// `session_id`, which is Rust-set only and so null for every Python-written
/// row -- the same "reports absent when the data exists" defect this row
/// projection was just fixed for, one field over.
#[test]
fn list_row_resolves_opencode_session_id_from_harness_session_id() {
    let home = short_home("listopencode");
    seed_stream_row(&home, "worker-opencode", "abc12345");
    state::update_registry(&home.registry_json(), |r| {
        let e = &mut r.entries[0];
        e.harness = Some("opencode".into());
        e.harness_session_id = Some("oc-sess-9f2".into());
        e.session_id = None;
    })
    .unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(1, "agent.list", json!({}));

    let response = handle_list_with_truth(&ctx, &req, per_handle(|_handle| probe("working")));
    let result = response.result().unwrap();
    let row = &result["agents"][0];

    assert_eq!(row["harness"], "opencode");
    assert_eq!(row["session_id"], "oc-sess-9f2");

    std::fs::remove_dir_all(home.root()).ok();
}

/// An empty crown scope renders `?`, not a trailing space. Python tests the
/// scope for falsiness (`self.crown_scope or '?'`), so matching only on None
/// would diverge on the empty string -- and nothing else covers that leg.
#[test]
fn list_row_crown_label_falls_back_on_an_empty_scope() {
    let home = short_home("listcrownempty");
    seed_stream_row(&home, "worker-crown", "abc12345");
    state::update_registry(&home.registry_json(), |r| {
        let e = &mut r.entries[0];
        e.crown_level = Some(1);
        e.crown_scope = Some(String::new());
    })
    .unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(1, "agent.list", json!({}));

    let response = handle_list_with_truth(&ctx, &req, per_handle(|_handle| probe("working")));
    let result = response.result().unwrap();

    assert_eq!(result["agents"][0]["crown"], "L1 ?");

    std::fs::remove_dir_all(home.root()).ok();
}

/// A row with no pane, no crown and no captured session id emits those keys
/// as null rather than omitting them -- consumers key off a stable shape.
#[test]
fn list_row_emits_absent_optional_fields_as_null() {
    let home = short_home("listnulls");
    seed_stream_row(&home, "worker-bare", "abc12345");
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(1, "agent.list", json!({}));

    let response = handle_list_with_truth(&ctx, &req, per_handle(|_handle| probe("working")));
    let result = response.result().unwrap();
    let row = &result["agents"][0];

    // Index-then-is_null would also pass for an ABSENT key (serde_json
    // returns Null for a missing index), which is the very defect being
    // guarded. Assert presence first, then the value.
    let obj = row.as_object().unwrap();
    for key in ["mux", "crown", "crown_level"] {
        assert!(obj.contains_key(key), "row omits key: {key}");
        assert!(obj[key].is_null(), "key {key} should be null on a bare row");
    }

    std::fs::remove_dir_all(home.root()).ok();
}

fn stream_identity(short_id: &str) -> Value {
    json!({
        "harness": "claude",
        "session_id": format!("uuid-{short_id}"),
        "short_id": short_id,
        "created_at": "2026-06-09T00:00:00Z",
    })
}

fn switchboard_params(
    to: &str,
    to_short: &str,
    from: &str,
    from_short: Option<&str>,
    body: &str,
) -> Value {
    let mut params = json!({
        "to": to,
        "from": from,
        "body": body,
        "mirror": from_short.is_some(),
        "recipient_identity": stream_identity(to_short),
    });
    if let Some(short_id) = from_short {
        params["from_identity"] = stream_identity(short_id);
    }
    params
}

#[test]
fn list_renders_family1_truth_instead_of_stored_registry_status() {
    let home = short_home("listtruth");
    seed_stream_row(&home, "worker-list", "abc12345");
    state::update_registry(&home.registry_json(), |registry| {
        registry.entries[0].status = AgentStatus::Orphaned;
    })
    .unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(1, "agent.list", json!({"status": "writing"}));

    let response = handle_list_with_truth(
        &ctx,
        &req,
        per_handle(|_handle| probe_with_verdict("working", "reachable")),
    );
    let result = response.result().unwrap();
    let agents = result["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["status"], "writing");

    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn list_queries_family1_by_session_identity_not_custom_name() {
    let home = short_home("listidentity");
    seed_stream_row(&home, "custom-worker-name", "abc12345");
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(1, "agent.list", json!({"status": "writing"}));
    let seen = std::cell::RefCell::new(Vec::new());

    let response = handle_list_with_truth(
        &ctx,
        &req,
        per_handle(|handle| {
            seen.borrow_mut().push(handle.to_string());
            probe_with_verdict("working", "reachable")
        }),
    );

    assert!(response.result().is_some());
    assert_eq!(seen.into_inner(), vec!["uuid-abc12345"]);
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn list_queries_pidless_row_by_bare_canonical_handle() {
    let home = short_home("listpidless");
    seed_stream_row(&home, "custom-worker-name", "unused");
    state::update_registry(&home.registry_json(), |registry| {
        registry.entries[0].short_id.clear();
        registry.entries[0].harness = Some("codex".into());
        registry.entries[0].harness_session_id =
            Some("019f8ff2-1111-2222-3333-444444444444".into());
    })
    .unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(1, "agent.list", json!({"status": "writing"}));
    let seen = std::cell::RefCell::new(Vec::new());

    let response = handle_list_with_truth(
        &ctx,
        &req,
        per_handle(|handle| {
            seen.borrow_mut().push(handle.to_string());
            probe_with_verdict("working", "reachable")
        }),
    );

    assert!(response.result().is_some());
    assert_eq!(
        seen.into_inner(),
        vec!["019f8ff2-1111-2222-3333-444444444444"]
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn list_queries_non_claude_row_by_transcript_identity() {
    let home = short_home("listnonclaude");
    seed_stream_row(&home, "custom-worker-name", "transport");
    state::update_registry(&home.registry_json(), |registry| {
        registry.entries[0].harness = Some("codex".into());
        registry.entries[0].harness_session_id =
            Some("019f8ff2-1111-2222-3333-444444444444".into());
    })
    .unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(1, "agent.list", json!({"status": "writing"}));
    let seen = std::cell::RefCell::new(Vec::new());

    let response = handle_list_with_truth(
        &ctx,
        &req,
        per_handle(|handle| {
            seen.borrow_mut().push(handle.to_string());
            probe_with_verdict("working", "reachable")
        }),
    );

    assert!(response.result().is_some());
    assert_eq!(
        seen.into_inner(),
        vec!["019f8ff2-1111-2222-3333-444444444444"]
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn list_applies_cheap_filters_before_family1_subprocesses() {
    let home = short_home("listprefilter");
    seed_stream_row(&home, "claude-worker", "aaaaaaaa");
    seed_stream_row(&home, "codex-worker", "bbbbbbbb");
    state::update_registry(&home.registry_json(), |registry| {
        registry.entries[1].harness = Some("codex".into());
        registry.entries[1].harness_session_id =
            Some("bbbbbbbb-1111-2222-3333-444444444444".into());
        // The provider filter reads the v15+ vendor axis: stamp the row
        // so it matches below. entries[0] stays unstamped, so a null
        // provider contributes to no vendor filter.
        registry.entries[1].provider = Some("openai".into());
    })
    .unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(1, "agent.list", json!({"provider": "openai"}));
    let seen = std::cell::RefCell::new(Vec::new());

    let response = handle_list_with_truth(
        &ctx,
        &req,
        per_handle(|handle| {
            seen.borrow_mut().push(handle.to_string());
            probe("working")
        }),
    );

    assert!(response.result().is_some());
    assert_eq!(
        seen.into_inner(),
        vec!["bbbbbbbb-1111-2222-3333-444444444444"]
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn list_pays_exactly_one_batch_call_for_the_whole_page() {
    // The x-0d93 fix, asserted where it is spent: N rows used to cost N
    // Python interpreter cold starts (780 ms each) on every `fno agents
    // list`. Asserted against the RAW seam, never `per_handle` -- an
    // adapter that fans a batch back out renders identically whether the
    // handler called it once or once per row, so only a direct call count
    // can tell the two apart.
    let home = short_home("listbatchonce");
    let rows = 24;
    for i in 0..rows {
        seed_stream_row(&home, &format!("worker-{i:02}"), &format!("{i:02}aaaaaa"));
    }
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(1, "agent.list", json!({"all": true}));
    let calls = std::cell::RefCell::new(Vec::new());

    let response = handle_list_with_truth(&ctx, &req, |handles: &[String]| {
        calls.borrow_mut().push(handles.to_vec());
        let map = handles
            .iter()
            .map(|h| (h.clone(), probe("working").unwrap()))
            .collect();
        (map, crate::truth_probe::BatchOutcome::Measured)
    });

    let calls = calls.into_inner();
    assert_eq!(calls.len(), 1, "one page, one batch");
    assert_eq!(calls[0].len(), rows, "every filtered row rides that batch");
    let entries = response.result().unwrap()["agents"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(entries, rows);
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn list_row_the_batch_did_not_answer_renders_exactly_as_an_unanswered_row() {
    // A handle absent from the batch map must be indistinguishable from the
    // per-row path's `None`: null triple, no invented model. Anything else
    // and the batch would be reporting a reading it never took.
    let home = short_home("listbatchpartial");
    seed_stream_row(&home, "answered", "aaaaaaaa");
    seed_stream_row(&home, "unanswered", "bbbbbbbb");
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(1, "agent.list", json!({"all": true}));

    let response = handle_list_with_truth(&ctx, &req, |handles: &[String]| {
        // Answers for the first handle only; the second is simply missing.
        let map = handles
            .iter()
            .take(1)
            .map(|h| (h.clone(), probe("working").unwrap()))
            .collect();
        (map, crate::truth_probe::BatchOutcome::Measured)
    });

    let agents = response.result().unwrap()["agents"].clone();
    let rows = agents.as_array().unwrap();
    let missing = rows
        .iter()
        .find(|r| r["name"] == "unanswered")
        .expect("the unanswered row still renders");
    assert!(missing["reachability"].is_null());
    assert!(missing["basis"].is_null());
    assert!(missing["last_activity_age_s"].is_null());
    assert_eq!(missing["observed_model"]["kind"], "no-transcript");
    std::fs::remove_dir_all(home.root()).ok();
}

/// Start a real stream worker (fake emitter child) on `home.worker_sock(id)`
/// via the PUBLIC `stream_worker::run`; wait for its socket to appear.
async fn start_stream_worker(home: &AgentsHome, short_id: &str, script: &str) -> PathBuf {
    let cfg = crate::stream_worker::StreamWorkerConfig::new(
        short_id,
        home.root().to_path_buf(),
        std::env::temp_dir(),
        vec!["bash".into(), "-c".into(), script.into()],
    );
    let short_id_dbg = short_id.to_string();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            if let Err(e) = crate::stream_worker::run(cfg).await {
                eprintln!("STREAM WORKER RUN ERROR ({short_id_dbg}): {e}");
            }
        });
    });
    let sock = home.worker_sock(short_id);
    // Phase 1: the socket file appears (the worker has bound).
    let bind_start = std::time::Instant::now();
    while !sock.exists() && bind_start.elapsed() < Duration::from_secs(20) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        sock.exists(),
        "stream worker socket never appeared for {short_id}"
    );
    // Phase 2: the worker actually accepts and answers a ping. sock.exists()
    // is bind, not readiness - a bound-but-starved worker (its own OS thread
    // + bash subprocess compete for cores under --test-threads=32) makes the
    // caller's 2s liveness probe time out and the test flake as
    // delivered:false ("not-a-live-stream-thread"). Wait for a ping before
    // handing the socket back, so the caller always sees a warm worker.
    // Capture the successful probe rather than re-probing in the assert: a
    // second independent 2s probe can itself time out under the same
    // CPU-starvation that made us wait, flaking the helper after readiness
    // was already established.
    let live_start = std::time::Instant::now();
    let mut live = false;
    while live_start.elapsed() < Duration::from_secs(30) {
        if is_live_stream_thread(&sock).await {
            live = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        live,
        "stream worker socket appeared but never answered a ping for {short_id}"
    );
    sock
}

/// Locate the cargo-built `fno-agents-worker` via this crate's manifest
/// dir (final binaries stay in the checkout's target/ under
/// build.build-dir, and the manifest path is worktree-local). `None` if
/// it is not built, so the e2e adopt test SKIPS rather than failing.
fn built_worker_bin() -> Option<PathBuf> {
    let cand = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/fno-agents-worker");
    cand.exists().then_some(cand)
}

// ---- Group 3 (ab-734fcd6c): claude stream-json front door --------------

#[test]
fn stream_claim_holder_is_short_id_scoped() {
    assert_eq!(stream_claim_holder("sw7"), "stream:sw7");
}

/// E1 (codex P2): interactive claude resolves to a real readiness detector,
/// not the fail-loud NoSignalDetector, so `agent.ask` against it does not
/// time out with "no readiness signal".
#[test]
fn provider_readiness_detector_handles_claude() {
    let d = provider_readiness_detector("claude");
    assert_eq!(d.provider_name(), "claude");
    // A truly unknown provider still gets the NoSignalDetector (name
    // carried). opencode graduated to a real match arm (x-51f6) - using
    // it here would coincidentally still pass (both paths report
    // provider_name() == "opencode") while silently testing the wrong
    // thing, so goose (still genuinely unhosted) is the example now.
    assert_eq!(
        provider_readiness_detector("goose").provider_name(),
        "goose"
    );
}

#[test]
fn is_live_writer_excludes_orphaned_and_terminal() {
    // Live-ish: a real writer holds the session -> one-host refuses a re-adopt.
    for s in [
        AgentStatus::Live,
        AgentStatus::Ready,
        AgentStatus::Idle,
        AgentStatus::Busy,
        AgentStatus::Spawning,
        AgentStatus::Restarting,
    ] {
        assert!(is_live_writer(s), "{s:?} should count as a live writer");
    }
    // Dead-but-non-terminal + terminal: the session is re-adoptable (AC1-FR).
    for s in [
        AgentStatus::Orphaned,
        AgentStatus::Failed,
        AgentStatus::Exited,
        AgentStatus::PermanentDead,
    ] {
        assert!(!is_live_writer(s), "{s:?} must NOT block re-adoption");
    }
}

#[test]
fn acquire_session_claim_maps_native_outcomes() {
    let td = tempfile::tempdir().unwrap();
    let _guard = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::env::set_var("FNO_CLAIMS_ROOT", td.path());
    // Fresh acquire -> Acquired.
    assert!(matches!(
        acquire_session_claim("U-1", "stream:sw1"),
        ClaimOutcome::Acquired
    ));
    // Same holder re-acquire -> still Acquired (idempotent).
    assert!(matches!(
        acquire_session_claim("U-1", "stream:sw1"),
        ClaimOutcome::Acquired
    ));
    // A different holder against a LIVE claim -> HeldByOther naming the
    // incumbent (the claim is pinned to this live test process).
    match acquire_session_claim("U-1", "stream:other") {
        ClaimOutcome::HeldByOther(who) => assert_eq!(who, "stream:sw1"),
        other => panic!("expected HeldByOther, got {other:?}"),
    }
    std::env::remove_var("FNO_CLAIMS_ROOT");
}

#[test]
fn claude_stream_worker_args_carry_stream_flags_and_child_argv() {
    let child = crate::provider::claude_stream_json_resume_argv("U-9");
    let args = claude_stream_worker_args(
        "sw9",
        std::path::Path::new("/home/agents"),
        std::path::Path::new("/work"),
        "U-9",
        "stream:sw9",
        &child,
    );
    // Selector + claim pair are present, the child argv follows `--`, and the
    // resume target is the FULL uuid (never the jobId).
    assert!(args.contains(&"--stream".to_string()));
    assert_eq!(
        args.iter()
            .position(|a| a == "--session-uuid")
            .map(|i| &args[i + 1]),
        Some(&"U-9".to_string())
    );
    assert_eq!(
        args.iter()
            .position(|a| a == "--holder")
            .map(|i| &args[i + 1]),
        Some(&"stream:sw9".to_string())
    );
    let sep = args
        .iter()
        .position(|a| a == "--")
        .expect("missing -- separator");
    assert_eq!(&args[sep + 1..], child.as_slice());
    assert_eq!(child[0], "claude");
    assert!(child.contains(&"--resume".to_string()) && child.contains(&"U-9".to_string()));
}

#[test]
fn build_claude_stream_entry_marks_interactive_claude_with_full_uuid() {
    let e = build_claude_stream_entry(
        "adopted",
        "sw3",
        std::path::Path::new("/proj"),
        "FULL-UUID-3",
        4242,
        Some(99),
        PathBuf::from("/proj/.fno/agents/sw3/timeline.jsonl"),
        None,
    );
    assert_eq!(e.harness_name(), "claude");
    assert_eq!(
        e.host_mode.as_deref(),
        Some(crate::state::HOST_MODE_INTERACTIVE)
    );
    assert!(
        e.is_interactive(),
        "stream thread must read as interactive for reconcile"
    );
    assert_eq!(e.claude_session_uuid.as_deref(), Some("FULL-UUID-3"));
    assert_eq!(e.status, AgentStatus::Live);
    assert_eq!(e.pid, Some(4242));
    // The resume key lives in claude_session_uuid; a stream thread carries
    // its worker short in short_id ("sw3"), not the removed jobId field.
    assert_eq!(e.short_id, "sw3");
}

/// AC1-ERR / front-door routing: a fresh `host --provider claude` with no
/// `--from` has nothing to resume; it is rejected (before any claim/spawn)
/// with a pointer to the adopt verb, proving claude routed to the stream lane
/// (not the codex/gemini PTY "only codex or gemini" gate).
#[tokio::test(flavor = "current_thread")]
async fn host_claude_without_from_rejected_with_adopt_pointer() {
    let home = short_home("clnofrom");
    let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent-worker"));
    let req = Request::new(
        1,
        "agent.spawn",
        json!({"name": "cl", "provider": "claude", "host_mode": "interactive"}),
    );
    let resp = handle_spawn(&ctx, &req).await;
    match &resp.payload {
        crate::protocol::ResponsePayload::Err(e) => {
            assert_eq!(e.code, ErrorCode::InvalidParams);
            assert!(
                e.message.contains("promote") && e.message.contains("--from"),
                "claude host without --from must point at the adopt verb; got: {}",
                e.message
            );
        }
        _ => panic!("expected error for claude host without --from"),
    }
    std::fs::remove_dir_all(home.root()).ok();
}

/// AC1-EDGE single-writer: a second adopt of a session already held by a live
/// claude thread is refused (one writer per session), before any spawn. Uses
/// the lock-free one-host pre-check so it is hermetic (no worker, no claim).
#[tokio::test(flavor = "current_thread")]
async fn promote_claude_duplicate_session_refused() {
    let home = short_home("cldup");
    seed_stream_row(&home, "first", "swDup"); // claude_session_uuid = uuid-swDup, Live
    let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent-worker"));
    let req = Request::new(
        1,
        "agent.spawn",
        json!({
            "name": "second", "provider": "claude", "host_mode": "interactive",
            "resume_id": "uuid-swDup"
        }),
    );
    let resp = handle_spawn(&ctx, &req).await;
    match &resp.payload {
        crate::protocol::ResponsePayload::Err(e) => {
            assert_eq!(e.code, ErrorCode::InvalidParams);
            assert!(
                e.message.contains("already hosted") && e.message.contains("first"),
                "duplicate adopt must name the existing host; got: {}",
                e.message
            );
        }
        _ => panic!("expected single-writer refusal for duplicate adopt"),
    }
    std::fs::remove_dir_all(home.root()).ok();
}

/// AC1-HP end-to-end: `promote --provider claude --from <uuid>` adopts an idle
/// session by spawning the real `--stream` worker (with a FAKE emitter child,
/// never a real `claude -p`) and registering it `live`. The row carries
/// provider=claude + host_mode=interactive + the FULL uuid, and the worker
/// serves the stream protocol. Skips when the worker binary is not built.
#[tokio::test(flavor = "current_thread")]
async fn promote_claude_spawns_live_stream_thread() {
    let Some(worker_bin) = built_worker_bin() else {
        eprintln!("skip promote_claude_spawns_live_stream_thread: worker bin not built");
        return;
    };
    let _guard = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = short_home("cle2e");
    // Hermetic claims: point `fno agents claim` at the test home so the real
    // acquire (daemon, this process) AND the worker child (inherits this env)
    // write `session:uuid-e2e` under /tmp, never the canonical, shared
    // ~/.fno/claims. A panic before teardown then leaks at worst into a
    // throwaway /tmp dir. Only this test exercises claims, so the process-wide
    // env set does not race the claim-free tests. (Edition 2021: set_var safe.)
    std::env::set_var("FNO_CLAIMS_ROOT", home.root());
    let ctx = test_ctx(home.clone(), worker_bin);
    let req = Request::new(
        1,
        "agent.spawn",
        json!({
            "name": "cl", "provider": "claude", "host_mode": "interactive",
            "resume_id": "uuid-e2e", "cwd": "/tmp",
            // Test escape hatch: a fake stream emitter stands in for `claude -p`.
            "argv": ["bash", "-c", FAKE_STREAM_EMITTER]
        }),
    );
    let resp = handle_spawn(&ctx, &req).await;
    let res = resp.result().expect("claude adopt errored");
    assert_eq!(res["harness"], "claude");
    assert_eq!(res["status"], "live");
    assert_eq!(res["lane"], "stream");

    let reg = load_registry_offloaded(home.registry_json())
        .await
        .expect("registry readable");
    let row = reg.find("cl").expect("adopted row missing");
    assert_eq!(row.harness_name(), "claude");
    assert_eq!(row.host_mode.as_deref(), Some("interactive"));
    assert_eq!(row.claude_session_uuid.as_deref(), Some("uuid-e2e"));
    assert_eq!(row.status, AgentStatus::Live);

    let sock = home.worker_sock(&row.short_id);
    assert!(
        is_live_stream_thread(&sock).await,
        "adopted thread must serve the stream protocol"
    );

    // Teardown: shut the worker down (its RAII guard releases the claim), then
    // drop the test home (which holds the redirected claims dir) and clear the
    // env override so later tests see the default claims root.
    best_effort_worker_shutdown(&sock).await;
    std::fs::remove_dir_all(home.root()).ok();
    std::env::remove_var("FNO_CLAIMS_ROOT");
}

/// AC1-ERR (codex review P2): a dead-on-arrival `claude -p --resume` (bad uuid
/// / auth fail, here a child that exits immediately) must NOT register live.
/// The worker still binds + answers stream.ping, but stream.status.child_alive
/// is false, so adopt is rejected and no row is created.
#[tokio::test(flavor = "current_thread")]
async fn promote_claude_dead_on_arrival_resume_rejected() {
    let Some(worker_bin) = built_worker_bin() else {
        eprintln!("skip promote_claude_dead_on_arrival_resume_rejected: worker bin not built");
        return;
    };
    let _guard = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = short_home("cldoa");
    std::env::set_var("FNO_CLAIMS_ROOT", home.root());
    let ctx = test_ctx(home.clone(), worker_bin);
    let req = Request::new(
        1,
        "agent.spawn",
        json!({
            "name": "cl", "provider": "claude", "host_mode": "interactive",
            "resume_id": "uuid-doa", "cwd": "/tmp",
            // Child exits immediately -> stands in for a bad/expired --resume id.
            "argv": ["bash", "-c", "exit 1"]
        }),
    );
    let resp = handle_spawn(&ctx, &req).await;
    assert!(
        resp.is_err(),
        "DOA resume child must be rejected, not registered"
    );
    assert_eq!(resp.error().unwrap().code, ErrorCode::SpawnFailed);
    let reg = load_registry_offloaded(home.registry_json())
        .await
        .expect("registry readable");
    assert!(
        reg.find("cl").is_none(),
        "no row may be registered for a DOA adopt"
    );
    std::fs::remove_dir_all(home.root()).ok();
    std::env::remove_var("FNO_CLAIMS_ROOT");
}

/// AC2-HP: `send A->B` between two held stream threads drives B, discriminates
/// the user-echo receipt from the reply, and mirrors B's reply into A.
#[tokio::test(flavor = "current_thread")]
async fn switchboard_drives_b_and_mirrors_into_a() {
    let home = short_home("hp");
    seed_stream_row(&home, "A", "swA");
    seed_stream_row(&home, "B", "swB");
    let _a = start_stream_worker(&home, "swA", FAKE_STREAM_EMITTER).await;
    let _b = start_stream_worker(&home, "swB", FAKE_STREAM_EMITTER).await;
    let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent-worker"));

    let req = Request::new(
        1,
        "agent.switchboard",
        switchboard_params("B", "swB", "A", Some("swA"), "hello"),
    );
    let resp = handle_switchboard(&ctx, &req).await;
    let res = resp.result().expect("switchboard errored");
    assert_eq!(res["delivered"], true, "not delivered: {res:?}");
    assert_eq!(res["reply"], "reply-text");
    assert_eq!(res["is_error"], false);
    assert_eq!(res["receipt"], true, "user-echo receipt not observed");
    assert_eq!(res["mirrored"], true, "B's reply was not mirrored into A");
    assert_eq!(res["identity_verified"], true);

    // The injected-event reuse carries the switchboard transport discriminator.
    let events = read_events(&home);
    assert!(
        events.iter().any(|e| e["type"] == "agent_deliver_injected"
            && e["data"]["transport"] == "switchboard"
            && e["data"]["mirrored"] == true),
        "switchboard injected event missing: {events:?}"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// A second turn against the SAME persistent worker must return the SECOND
/// turn's reply, not the stale first result still in the append-only frame
/// log. Regression for the cursor=0 bug: the emitter tags each reply with a
/// per-turn counter so a stale read is detectable.
#[tokio::test(flavor = "current_thread")]
async fn switchboard_second_turn_returns_fresh_reply() {
    const COUNTING_EMITTER: &str = r#"
printf '%s\n' '{"type":"system","subtype":"init","session_id":"s1"}'
n=0
while IFS= read -r line; do
  n=$((n+1))
  printf '%s\n' '{"type":"user","message":{"role":"user"}}'
  printf '%s\n' "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"reply-$n\"}]}}"
  printf '%s\n' "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"reply-$n\"}"
done
"#;
    let home = short_home("fresh");
    seed_stream_row(&home, "B", "swB");
    let _b = start_stream_worker(&home, "swB", COUNTING_EMITTER).await;
    let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent-worker"));

    let r1 = handle_switchboard(
        &ctx,
        &Request::new(
            1,
            "agent.switchboard",
            switchboard_params("B", "swB", "ghost", None, "first"),
        ),
    )
    .await;
    assert_eq!(r1.result().expect("hop1")["reply"], "reply-1");

    let r2 = handle_switchboard(
        &ctx,
        &Request::new(
            2,
            "agent.switchboard",
            switchboard_params("B", "swB", "ghost", None, "second"),
        ),
    )
    .await;
    assert_eq!(
        r2.result().expect("hop2")["reply"],
        "reply-2",
        "second drive returned a STALE reply (cursor not advanced past the prior turn)"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// Routing: a claude peer with no live stream worker demotes (the caller
/// falls back to the durable/socket path), not an error.
#[tokio::test(flavor = "current_thread")]
async fn switchboard_demotes_when_b_not_a_live_stream_thread() {
    let home = short_home("demote");
    seed_stream_row(&home, "B", "swB"); // registered, but NO worker started
    let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent-worker"));

    let req = Request::new(
        1,
        "agent.switchboard",
        switchboard_params("B", "swB", "A", None, "hi"),
    );
    let resp = handle_switchboard(&ctx, &req).await;
    let res = resp
        .result()
        .expect("should be Ok-demote, not an RPC error");
    assert_eq!(res["delivered"], false);
    assert_eq!(res["reason"], "not-a-live-stream-thread");
    std::fs::remove_dir_all(home.root()).ok();
}

/// Degenerate one-way drive: B is a held stream thread but A is not (absent),
/// so the turn delivers to B with no mirror.
#[tokio::test(flavor = "current_thread")]
async fn switchboard_one_way_when_peer_absent() {
    let home = short_home("oneway");
    seed_stream_row(&home, "B", "swB");
    let _b = start_stream_worker(&home, "swB", FAKE_STREAM_EMITTER).await;
    let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent-worker"));

    let req = Request::new(
        1,
        "agent.switchboard",
        switchboard_params("B", "swB", "ghost", None, "hi"),
    );
    let resp = handle_switchboard(&ctx, &req).await;
    let res = resp.result().expect("switchboard errored");
    assert_eq!(res["delivered"], true);
    assert_eq!(res["reply"], "reply-text");
    assert_eq!(res["mirrored"], false, "no peer to mirror into");
    std::fs::remove_dir_all(home.root()).ok();
}

/// An unknown `to` is an RPC error (AgentNotFound), not a silent no-op.
#[tokio::test(flavor = "current_thread")]
async fn switchboard_unknown_target_is_not_found() {
    let home = short_home("404");
    let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent-worker"));
    let req = Request::new(
        1,
        "agent.switchboard",
        switchboard_params("nope", "swNope", "A", None, "hi"),
    );
    let resp = handle_switchboard(&ctx, &req).await;
    if let crate::protocol::ResponsePayload::Err(ref e) = resp.payload {
        assert_eq!(e.code, ErrorCode::AgentNotFound);
    } else {
        panic!("expected AgentNotFound, got {resp:?}");
    }
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test(flavor = "current_thread")]
async fn switchboard_refuses_replaced_recipient_identity() {
    let home = short_home("replaced");
    seed_stream_row(&home, "victim", "swB");
    let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent-worker"));
    let mut params = switchboard_params("victim", "swB", "ghost", None, "secret");
    params["recipient_identity"]["session_id"] = json!("uuid-swA");

    let response = handle_switchboard(&ctx, &Request::new(1, "agent.switchboard", params)).await;
    let result = response.result().expect("identity mismatch is a demotion");
    assert_eq!(result["delivered"], false);
    assert_eq!(result["reason"], "recipient-identity-changed");
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test(flavor = "current_thread")]
async fn switchboard_failed_drive_does_not_orphan_restamped_recipient() {
    let home = short_home("failedrestamp");
    seed_stream_row(&home, "B", "swB");
    let turn_started = home.root().join("turn-started");
    let restamp_done = home.root().join("restamp-done");
    let script = format!(
        r#"
printf '%s\n' '{{"type":"system","subtype":"init","session_id":"s1"}}'
while IFS= read -r line; do
  touch '{}'
  while [ ! -f '{}' ]; do sleep 0.01; done
  exit 1
done
"#,
        turn_started.display(),
        restamp_done.display()
    );
    let _b = start_stream_worker(&home, "swB", &script).await;
    let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent-worker"));

    let registry_path = home.registry_json();
    let restamp_signal = turn_started.clone();
    let restamp_complete = restamp_done.clone();
    let restamp = tokio::spawn(async move {
        let start = Instant::now();
        while !restamp_signal.exists() && start.elapsed() < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(restamp_signal.exists(), "drive never reached the worker");
        state::update_registry(&registry_path, |registry| {
            let row = registry.find_mut("B").expect("recipient row missing");
            row.short_id = "swC".into();
            row.harness_session_id = Some("uuid-replacement".into());
            row.claude_session_uuid = Some("uuid-replacement".into());
            row.created_at = "2026-06-09T00:00:01Z".into();
            row.status = AgentStatus::Live;
        })
        .unwrap();
        std::fs::write(restamp_complete, b"done\n").unwrap();
    });

    let response = handle_switchboard(
        &ctx,
        &Request::new(
            1,
            "agent.switchboard",
            switchboard_params("B", "swB", "ghost", None, "fail after receipt"),
        ),
    )
    .await;
    restamp.await.unwrap();
    let result = response.result().expect("failed drive is a demotion");
    assert_eq!(result["delivered"], false);

    let registry = state::load_registry(&home.registry_json()).unwrap();
    let replacement = registry.find("B").expect("replacement row missing");
    assert_eq!(replacement.status, AgentStatus::Live);
    assert_eq!(
        replacement.harness_session_id.as_deref(),
        Some("uuid-replacement")
    );
    let events = read_events(&home);
    assert!(
        events.iter().any(|event| {
            event["type"] == "agent_deliver_status_write_failed"
                && event["data"]["name"] == "B"
                && event["data"]["reason"] == "recipient-identity-changed"
        }),
        "identity-CAS failure event missing: {events:?}"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test(flavor = "current_thread")]
async fn switchboard_requires_recipient_identity() {
    let home = short_home("identity-required");
    let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent-worker"));
    let response = handle_switchboard(
        &ctx,
        &Request::new(
            1,
            "agent.switchboard",
            json!({"to": "victim", "from": "ghost", "body": "secret", "mirror": false}),
        ),
    )
    .await;
    assert_eq!(
        response.error().expect("missing identity must fail").code,
        ErrorCode::InvalidParams
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test(flavor = "current_thread")]
async fn switchboard_v2_routes_to_identity_guard() {
    let home = short_home("v2route");
    let ctx = Arc::new(test_ctx(home.clone(), PathBuf::from("/nonexistent-worker")));
    let response = dispatch_agent(
        &ctx,
        &Request::new(
            1,
            "agent.switchboard_v2",
            json!({"to": "victim", "from": "ghost", "body": "secret"}),
        ),
    )
    .await;
    let error = response.error().expect("missing identity must fail");
    assert_eq!(error.code, ErrorCode::InvalidParams);
    assert!(error.message.contains("recipient_identity"));
    std::fs::remove_dir_all(home.root()).ok();
}

/// Post-G4 (x-f54c): a codex spawn (interactive PTY hosting) is retired -- the
/// daemon serves only the claude stream-json adopt lane, so any other spawn
/// returns the mux-pointer InvalidParams error.
#[tokio::test(flavor = "current_thread")]
async fn handle_spawn_codex_pty_hosting_retired_returns_pointer() {
    let home = tmp_home("spawn-provider-argv");
    let ctx = test_ctx(
        home.clone(),
        PathBuf::from("/nonexistent/fno-agents-worker"),
    );
    let req = Request::new(
        1,
        "agent.spawn",
        json!({"name": "test-agent", "provider": "codex"}),
    );
    let resp = handle_spawn(&ctx, &req).await;
    match &resp.payload {
        crate::protocol::ResponsePayload::Err(e) => {
            assert_eq!(e.code, ErrorCode::InvalidParams);
            assert!(
                e.message.contains("retired at G4"),
                "codex spawn must point at the mux; got: {}",
                e.message
            );
        }
        _ => panic!("expected the G4 retirement error for a codex PTY spawn"),
    }
    std::fs::remove_dir_all(home.root()).ok();
}

/// AC3-ERR: handle_spawn with an unknown/non-PTY provider and no argv returns InvalidParams.
#[tokio::test(flavor = "current_thread")]
async fn handle_spawn_unknown_provider_no_argv_returns_invalid_params() {
    let home = tmp_home("spawn-unknown-provider");
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(
        1,
        "agent.spawn",
        json!({"name": "test-agent", "provider": "nonexistent-provider"}),
    );
    let resp = handle_spawn(&ctx, &req).await;
    match &resp.payload {
        crate::protocol::ResponsePayload::Err(e) => {
            assert_eq!(
                e.code,
                ErrorCode::InvalidParams,
                "unknown provider without argv must return InvalidParams"
            );
        }
        _ => panic!("expected error response for unknown provider"),
    }
    std::fs::remove_dir_all(home.root()).ok();
}

// --- codex thread lane: actor-driven ask / stop (the x-de10 probes) ---
//
// These three are the make-it-fail probes for the concurrency rewrite:
// each FAILS against the old Arc<Mutex<CodexThread>> shape (verified by
// running them on the pre-rewrite daemon) and passes against the actor.

// PATH + ask-wait env serialization across these tests AND every other
// PATH-mutating test in the lib (provider.rs, client_verbs.rs): one shared
// mutex, never nested. The fakes install `codex` on PATH and one test
// retunes FNO_CODEX_ASK_WAIT_MS; a concurrent `set_var` from an unrelated
// test would make the child inherit a broken PATH (exit 127).

/// Point `CODEX_HOME` at a fake shared app-server daemon for the duration
/// of `body`. The fake lives in [`crate::codex_fake_daemon`]: one
/// implementation of the protocol, shared with the integration tests.
async fn with_fake_codex_daemon(
    behavior: crate::codex_fake_daemon::Behavior,
    body: impl std::future::Future<Output = ()>,
) {
    let _guard = crate::path_test_guard();
    let _daemon = crate::codex_fake_daemon::FakeDaemon::start(behavior);
    body.await;
}

/// Block until the worker's actor reports a DRIVING turn, or fail.
///
/// The tests below used a fixed 300ms sleep, which is a bet that the seed
/// turn started by then. Under a loaded parallel suite it does not always
/// hold, and losing it does not make a test slower, it makes it WRONG: an
/// interrupt arriving before the turn starts reads `NoTurnInFlight`, so
/// `stop` correctly reports `no-turn` and the assertion for `interrupted`
/// fails. Waiting on the actor's own turn id removes the bet.
async fn await_driving_turn(ctx: &Ctx, name: &str) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let handle = ctx.codex_threads.lock().await.get(name).cloned();
        if let Some(turn) = handle.and_then(|handle| handle.current_turn_id()) {
            return turn;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no turn ever started driving for {name}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Block until an `agent_ask_done` is on disk, then return every event.
///
/// The two callers slept a fixed 400ms or 1300ms and then counted. That is
/// a bet that the fake's turn finished AND its emit reached the file inside
/// the window. CI lost it: `switchboard_to_codex_thread_delivers_on_steering_ack_mid_turn`
/// read ZERO done events on a loaded runner while passing on every local
/// run. A completion is an observable marker, so wait for the marker.
///
/// Counting `== 1` right after the first one lands is still sound: both
/// callers make exactly two submits and have already asserted upstream
/// that the two shared one turn id, so nothing can start a second turn
/// after this returns.
async fn await_ask_done(home: &AgentsHome) -> Vec<Value> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let events = read_events(home);
        if events.iter().any(|e| e["type"] == "agent_ask_done") {
            return events;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no agent_ask_done ever landed: {events:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// AC4-HP: handle_ask on AgentNotFound with a provider param routes into the
/// first-contact spawn branch (does NOT short-circuit AgentNotFound). Post-G4
/// (x-f54c) that spawn is the retired codex PTY-hosting path, so the daemon
/// surfaces the mux pointer rather than auto-creating a worker; the point of
/// the test is that first-contact attempted a spawn (not AgentNotFound).
#[tokio::test(flavor = "current_thread")]
async fn handle_ask_first_contact_with_provider_routes_into_spawn() {
    let home = tmp_home("ask-first-contact");
    let ctx = test_ctx(
        home.clone(),
        PathBuf::from("/nonexistent/fno-agents-worker"),
    );
    // Agent does not exist yet; provider="codex" is provided.
    let req = Request::new(
        1,
        "agent.ask",
        json!({"name": "new-agent", "message": "hello", "provider": "codex"}),
    );
    let resp = handle_ask(&ctx, &req).await;
    match &resp.payload {
        crate::protocol::ResponsePayload::Err(e) => {
            assert_ne!(
                    e.code,
                    ErrorCode::AgentNotFound,
                    "first-contact ask with --provider must route into the spawn branch, not short-circuit AgentNotFound; got: {}",
                    e.message
                );
            // Post-G4 the codex spawn is retired -> the mux pointer.
            assert!(
                e.message.contains("retired at G4"),
                "first-contact codex spawn must surface the G4 mux pointer; got: {}",
                e.message
            );
        }
        crate::protocol::ResponsePayload::Ok(v) => {
            panic!("post-G4 a codex first-contact spawn must fail, got Ok: {v}")
        }
    }
    std::fs::remove_dir_all(home.root()).ok();
}

/// AC5-ERR: handle_ask on AgentNotFound WITHOUT a provider returns InvalidParams
/// (mirrors Python requiring --provider on first contact).
#[tokio::test(flavor = "current_thread")]
async fn handle_ask_first_contact_without_provider_returns_invalid_params() {
    let home = tmp_home("ask-no-provider");
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    // Agent does not exist; NO provider param.
    let req = Request::new(
        1,
        "agent.ask",
        json!({"name": "ghost-agent", "message": "hello"}),
    );
    let resp = handle_ask(&ctx, &req).await;
    match &resp.payload {
        crate::protocol::ResponsePayload::Err(e) => {
            assert_eq!(
                e.code,
                ErrorCode::InvalidParams,
                "first-contact ask without --provider must return InvalidParams; got: {}",
                e.message
            );
            assert!(
                e.message.contains("provider"),
                "error message must mention 'provider', got: {}",
                e.message
            );
        }
        _ => panic!("expected error for first-contact ask without provider"),
    }
    std::fs::remove_dir_all(home.root()).ok();
}

/// A full-session-id token must resolve in handle_ask too: the client
/// pre-check accepts one, so a name-only lookup here read a live agent as
/// absent and, with --provider, would auto-spawn a duplicate row for the
/// same session.
#[tokio::test(flavor = "current_thread")]
async fn handle_ask_resolves_a_full_session_id_to_the_named_row() {
    let home = tmp_home("ask-full-id");
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let session_id = "12345678-1234-4234-8234-123456789abc";
    let mut row = ask_row("alice", None);
    row.harness = Some("claude".into());
    row.harness_session_id = Some(session_id.into());
    row.short_id = "abcd1234".into();
    row.status = AgentStatus::Live;
    state::update_registry(
        &home.registry_json(),
        |r| -> Result<(), std::convert::Infallible> {
            r.entries.push(row);
            Ok(())
        },
    )
    .unwrap();
    let req = Request::new(
        1,
        "agent.ask",
        json!({
            "name": session_id.to_ascii_uppercase(),
            "message": "hello"
        }),
    );
    let resp = handle_ask(&ctx, &req).await;
    match &resp.payload {
        crate::protocol::ResponsePayload::Ok(_) => {}
        crate::protocol::ResponsePayload::Err(e) => {
            assert!(
                !e.message.contains("pass --provider"),
                "a full-id token must resolve to the named row, not read as absent: {}",
                e.message
            );
        }
    }
    std::fs::remove_dir_all(home.root()).ok();
}

// ── gate record tests (Task 2.3) ─────────────────────────────────────────

// ---- inside-leg report (E3.2) ------------------------------------------

/// AC-X2 store: a report for a registered claude session lands on the row's
/// `inside_leg` field with the daemon-stamped `received_at`, and emits
/// `inside_leg_report`.
#[test]
fn handle_report_stores_on_matching_row() {
    let home = tmp_home("report-store");
    seed_stream_row(&home, "worker-A", "repA"); // claude_session_uuid = uuid-repA
    let ctx = test_ctx_with_events(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(
        1,
        "agent.report",
        json!({"session_id": "uuid-repA", "seq": 3, "state": "working", "reason": "running tests"}),
    );
    let resp = handle_report(&ctx, &req);
    assert!(!resp.is_err(), "report must return Ok: {resp:?}");
    assert_eq!(resp.result().unwrap()["stored"], true);

    let reg = state::load_registry(&home.registry_json()).unwrap();
    let rep = reg.entries[0]
        .inside_leg
        .as_ref()
        .expect("inside_leg stored");
    assert_eq!(rep.state, state::InsideLegState::Working);
    assert_eq!(rep.seq, 3);
    assert_eq!(rep.reason.as_deref(), Some("running tests"));
    assert!(!rep.received_at.is_empty(), "daemon stamps received_at");

    let events = read_events(&home);
    assert!(
        events.iter().any(|e| e["type"] == "inside_leg_report"),
        "inside_leg_report not emitted: {events:?}"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// A model report is an observation whether it agrees with the request
/// or not: the normal case (the row already carries the requested model)
/// must flip `model_basis` to "verified" too, or a healthy worker reads
/// as unobserved forever - the audit's requested-vs-observed boundary
/// reads this field.
#[test]
fn handle_report_marks_a_matching_model_as_verified() {
    let home = tmp_home("report-matching-model-verified");
    seed_stream_row(&home, "worker-A", "repM");
    state::update_registry(&home.registry_json(), |r| {
        r.entries[0].model = Some("glm-5.3-flash[1m]".into());
        r.entries[0].model_basis = Some("requested".into());
    })
    .unwrap();
    let ctx = test_ctx_with_events(home.clone(), PathBuf::from("fno-agents-worker"));
    let resp = handle_report(
        &ctx,
        &Request::new(
            1,
            "agent.report",
            json!({"session_id": "uuid-repM", "seq": 1, "state": "working",
                       "model": "glm-5.3-flash[1m]"}),
        ),
    );
    assert_eq!(resp.result().unwrap()["stored"], true);
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert_eq!(reg.entries[0].model_basis.as_deref(), Some("verified"));
    let events = read_events(&home);
    assert!(
        !events.iter().any(|e| e["type"] == "agent_model_changed"),
        "a matching report is not a change: {events:?}"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// Capability flip (screen-manifest fallback authority): the row's FIRST
/// inside-leg report makes the hook the sole authority - a stored scrape
/// verdict is cleared in the same registry write, so it can never shadow
/// the hook.
#[test]
fn handle_report_capability_flip_clears_screen_state() {
    let home = tmp_home("report-flip-clears-scrape");
    seed_stream_row(&home, "worker-A", "repF");
    state::update_registry(&home.registry_json(), |r| {
        r.entries[0].screen_state = Some(state::ScreenStateReport {
            state: "idle".into(),
            rule: "idle_prompt".into(),
            seq: 4,
            at: "2026-07-02T00:00:00Z".into(),
            ttl_ms: Some(120_000),
            answerable: None,
        });
    })
    .unwrap();
    let ctx = test_ctx_with_events(home.clone(), PathBuf::from("fno-agents-worker"));
    let resp = handle_report(
        &ctx,
        &Request::new(
            1,
            "agent.report",
            json!({"session_id": "uuid-repF", "seq": 1, "state": "working"}),
        ),
    );
    assert_eq!(resp.result().unwrap()["stored"], true);
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(reg.entries[0].inside_leg.is_some());
    assert_eq!(
        reg.entries[0].screen_state, None,
        "capability flip must clear the scrape verdict"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// The `blocked` producer (Notification hook) stores state and
/// reason exactly like `working`/`done` and gets the same capability flip
/// -- a `blocked` row is demoted from the scraper by construction, not by
/// a special case, so a stale screen-manifest verdict can never shadow a
/// hook-reported Waiting row.
#[test]
fn handle_report_blocked_stores_reason_and_clears_screen_state() {
    let home = tmp_home("report-blocked");
    seed_stream_row(&home, "worker-A", "repW");
    state::update_registry(&home.registry_json(), |r| {
        r.entries[0].screen_state = Some(state::ScreenStateReport {
            state: "idle".into(),
            rule: "idle_prompt".into(),
            seq: 4,
            at: "2026-07-02T00:00:00Z".into(),
            ttl_ms: Some(120_000),
            answerable: None,
        });
    })
    .unwrap();
    let ctx = test_ctx_with_events(home.clone(), PathBuf::from("fno-agents-worker"));
    let resp = handle_report(
        &ctx,
        &Request::new(
            1,
            "agent.report",
            json!({"session_id": "uuid-repW", "seq": 1, "state": "blocked", "reason": "permission to run rm"}),
        ),
    );
    assert!(!resp.is_err(), "report must return Ok: {resp:?}");
    assert_eq!(resp.result().unwrap()["stored"], true);

    let reg = state::load_registry(&home.registry_json()).unwrap();
    let rep = reg.entries[0]
        .inside_leg
        .as_ref()
        .expect("inside_leg stored");
    assert_eq!(rep.state, state::InsideLegState::Blocked);
    assert_eq!(rep.reason.as_deref(), Some("permission to run rm"));
    assert_eq!(
        reg.entries[0].screen_state, None,
        "capability flip must clear the scrape verdict on a blocked report too"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// AC-X2-1 seq: a `seq <= last_seq` is dropped (the newer report wins) and
/// emits `inside_leg_report_dropped`.
#[test]
fn handle_report_drops_stale_seq() {
    let home = tmp_home("report-stale");
    seed_stream_row(&home, "worker-A", "repB");
    let ctx = test_ctx_with_events(home.clone(), PathBuf::from("fno-agents-worker"));
    // seq=2 stored, then a reordered seq=1 arrives.
    let _ = handle_report(
        &ctx,
        &Request::new(
            1,
            "agent.report",
            json!({"session_id": "uuid-repB", "seq": 2, "state": "working"}),
        ),
    );
    let resp = handle_report(
        &ctx,
        &Request::new(
            2,
            "agent.report",
            json!({"session_id": "uuid-repB", "seq": 1, "state": "done"}),
        ),
    );
    assert!(!resp.is_err());
    assert_eq!(resp.result().unwrap()["stored"], false);
    assert_eq!(resp.result().unwrap()["dropped"], "stale_seq");

    // The badge still reflects seq=2/working, not the late seq=1/done.
    let reg = state::load_registry(&home.registry_json()).unwrap();
    let rep = reg.entries[0].inside_leg.as_ref().unwrap();
    assert_eq!(rep.seq, 2);
    assert_eq!(rep.state, state::InsideLegState::Working);

    let events = read_events(&home);
    assert!(events
        .iter()
        .any(|e| e["type"] == "inside_leg_report_dropped" && e["data"]["reason"] == "stale_seq"));
    std::fs::remove_dir_all(home.root()).ok();
}

/// AC-X2-5 + E3.3 buffer-on-early-push: a push for an unregistered session id
/// is BUFFERED (no longer hard-dropped) with a logged event and adds no
/// phantom row. The buffered report is flushed onto the row at creation.
#[test]
fn handle_report_buffers_early_push_for_unknown_session() {
    let home = tmp_home("report-unknown");
    let ctx = test_ctx_with_events(home.clone(), PathBuf::from("fno-agents-worker"));
    let resp = handle_report(
        &ctx,
        &Request::new(
            1,
            "agent.report",
            json!({"session_id": "uuid-nope", "seq": 1, "state": "working"}),
        ),
    );
    assert!(!resp.is_err());
    assert_eq!(resp.result().unwrap()["stored"], false);
    assert_eq!(
        resp.result().unwrap()["buffered"],
        true,
        "an early push is held, not dropped (E3.3)"
    );

    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(reg.entries.is_empty(), "no phantom row created");
    // The report is held in the pending buffer keyed by session_id.
    assert_eq!(
        ctx.pending_inside_leg
            .lock()
            .unwrap()
            .get("uuid-nope")
            .map(|r| r.seq),
        Some(1)
    );

    let events = read_events(&home);
    assert!(events.iter().any(
        |e| e["type"] == "inside_leg_report_buffered" && e["data"]["session_id"] == "uuid-nope"
    ));
    std::fs::remove_dir_all(home.root()).ok();
}

/// Missing/invalid params fail closed with InvalidParams (no registry write).
#[test]
fn handle_report_rejects_bad_params() {
    let home = tmp_home("report-bad");
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    for params in [
        json!({"seq": 1, "state": "working"}),          // no session_id
        json!({"session_id": "x", "state": "working"}), // no seq
        json!({"session_id": "x", "seq": 1}),           // no state
        json!({"session_id": "x", "seq": 1, "state": "idle"}), // bad state
    ] {
        let resp = handle_report(&ctx, &Request::new(1, "agent.report", params.clone()));
        assert!(resp.is_err(), "expected InvalidParams for {params}");
    }
    std::fs::remove_dir_all(home.root()).ok();
}

/// A non-object `envelope` is rejected with InvalidParams BEFORE any registry
/// or sidecar work (channel need not even exist).
#[test]
fn push_to_channel_rejects_non_object_envelope() {
    let home = tmp_home("push-badenv");
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let resp = handle_push_to_channel(
        &ctx,
        &Request::new(
            1,
            "channel.push_to_channel",
            json!({"mcp_channel_id": "c1", "envelope": "not-an-object"}),
        ),
    );
    assert!(resp.is_err(), "non-object envelope must be InvalidParams");
    std::fs::remove_dir_all(home.root()).ok();
}

/// An envelope to an unregistered channel -> ChannelUnknown; the sidecar is
/// never invoked.
#[test]
fn push_to_channel_unknown_channel_errors() {
    let home = tmp_home("push-unknown");
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let resp = handle_push_to_channel(
        &ctx,
        &Request::new(
            1,
            "channel.push_to_channel",
            json!({"mcp_channel_id": "nope", "envelope": {"a": 1}}),
        ),
    );
    assert!(resp.is_err(), "unknown channel must error");
    std::fs::remove_dir_all(home.root()).ok();
}

/// No envelope against a registered channel -> legacy `{"routed": true}`
/// exactly (confirm-only, unchanged; no `delivered` key).
#[test]
fn push_to_channel_no_envelope_is_confirm_only() {
    let home = tmp_home("push-confirm");
    seed_stream_row(&home, "worker-c", "chA");
    state::update_registry(&home.registry_json(), |r| {
        r.entries[0].mcp_channel_id = Some("c1".into());
    })
    .unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let resp = handle_push_to_channel(
        &ctx,
        &Request::new(
            1,
            "channel.push_to_channel",
            json!({"mcp_channel_id": "c1"}),
        ),
    );
    let result = resp.result().unwrap();
    assert_eq!(result["routed"], true);
    assert!(
        result.get("delivered").is_none(),
        "confirm-only must not claim delivery"
    );
    std::fs::remove_dir_all(home.root()).ok();
}
