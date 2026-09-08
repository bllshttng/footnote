//! The codex thread lane's spawn/registry/resume tests, moved verbatim
//! into their own module (file budget: this file is far over the
//! shrink-only line; test motion is the sanctioned shrink). Covers
//! `spawn_codex_thread_lane`, `build_codex_thread_entry`,
//! `ensure_codex_thread_handle`, and the `agent.spawn --substrate thread`
//! dispatch family. Shared helpers (`tmp_home`, `rentry`, `test_ctx`, ...)
//! stay in the parent tests module and resolve through the glob.
use super::*;

// `thread_entry` stays in the parent tests module (used by the plan_reconcile
// liveness family too, not only this one) and reaches here through the glob.

#[test]
fn reconcile_leaves_a_hosted_codex_thread_untouched() {
    let entries = vec![thread_entry(
        "t-hosted",
        AgentStatus::Live,
        Some("/tmp/r.jsonl".into()),
    )];
    let (changes, _) = plan_reconcile(
        &entries,
        |_| Ok(false),
        || false,
        |_| true,
        |_| false,
        |_| true,               // thread_hosted: the daemon map names this row
        |_| false,              // rollout_exists (irrelevant while hosted)
        |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
        true,                   // roster readable: the flip needs a successful roster read
    );
    assert_eq!(
        changes[0].new_status, None,
        "a hosted thread is the daemon's own; the stale pid must not settle it"
    );
}

/// AC15: a row whose startup resume FAILED reads Orphaned after the
/// recovery pass, never Live-forever. The resume is made to fail
/// deterministically via a nonexistent cwd (app-server spawn cannot even
/// start there).
/// AC11: a yolo spawn stamps the posture on the row; the resume lane's
/// helper reads it back.
///
/// It drives a fake SHARED daemon. It used to install a stdio `codex` on
/// PATH and let the driver fork it. After the transport moved to the
/// shared daemon that fake was never reached: on a developer machine the
/// driver connected to the operator's REAL daemon and the test passed by
/// starting a real thread, and in CI, where no daemon runs, it panicked.
/// A test that reaches a live daemon is not a unit test, so it takes the
/// same fake every other one here does.
#[test]
fn build_codex_thread_entry_stamps_the_launch_posture() {
    let worktree = tempfile::tempdir().unwrap();
    let _guard = crate::path_test_guard();
    let start = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            // The fake must outlive the start: it owns CODEX_HOME.
            let _daemon = crate::codex_fake_daemon::FakeDaemon::start(
                crate::codex_fake_daemon::Behavior::quick().with_thread_id("thread-p"),
            );
            crate::codex_thread::CodexThread::start(worktree.path(), None, true, None)
                .await
                .expect("yolo thread starts")
        });
    let yolo = build_codex_thread_entry("t", worktree.path(), &start, None, None, true, None, None);
    assert_eq!(yolo.sandbox_posture.as_deref(), Some("danger-full-access"));
    assert!(
        entry_posture_is_full_access(&yolo)
            && yolo.fno_id.as_deref() == Some("thread-p")
            && yolo.mux.is_none()
    );
    let bounded =
        build_codex_thread_entry("t", worktree.path(), &start, None, None, false, None, None);
    assert_eq!(bounded.sandbox_posture.as_deref(), Some("workspace-write"));
    assert!(!entry_posture_is_full_access(&bounded));
    // A requested model stamps its basis on the row; an absent one
    // leaves the basis absent with it.
    let modeled = build_codex_thread_entry(
        "t",
        worktree.path(),
        &start,
        Some("gpt-5.6-sol"),
        None,
        false,
        None,
        None,
    );
    assert_eq!(modeled.model.as_deref(), Some("gpt-5.6-sol"));
    assert_eq!(modeled.model_basis.as_deref(), Some("requested"));
    assert_eq!(bounded.model_basis, None);
    // v25 positive marker: the route identity the spawn actually used,
    // read back non-empty from the minted row - the provider-outage
    // collector refuses evidence on a row whose axes are absent, so an
    // all-None stamp here would keep every daemon codex thread blind.
    assert_eq!(modeled.route_provider_id.as_deref(), Some("openai"));
    assert_eq!(modeled.model_name.as_deref(), Some("gpt-5.6-sol"));
    assert_eq!(modeled.account_record_id.as_deref(), Some("default"));
}

/// The RESOLVED posture is a separate column from the REQUESTED one, and
/// the yolo row is where that matters: the spawn asks for full access and
/// this fake, like the real app-server on the measurement that opened this
/// lane, reports no sandbox at all. A row that carried only
/// `sandbox_posture` would answer "danger-full-access" to the question
/// "what could this worker write", which is the misread the column ends.
///
/// The fake models no sandbox on purpose, so `unknown` is the branch it
/// can prove. It is asserted as a VALUE, never as a missing key: an absent
/// field reads the same as a full-access thread, and that ambiguity is the
/// defect, not the record of it.
#[test]
fn build_codex_thread_entry_records_the_resolved_posture_and_its_roots() {
    let worktree = tempfile::tempdir().unwrap();
    let _guard = crate::path_test_guard();
    let granted = worktree.path().join("state-root");
    std::fs::create_dir_all(&granted).unwrap();
    let granted_s = granted.to_string_lossy().into_owned();
    let start = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on({
            let granted_s = granted_s.clone();
            let cwd = worktree.path().to_path_buf();
            async move {
                let _daemon = crate::codex_fake_daemon::FakeDaemon::start(
                    crate::codex_fake_daemon::Behavior::quick().with_thread_id("thread-r"),
                );
                crate::codex_thread::CodexThread::start_with_state_dirs(
                    cwd,
                    None,
                    false,
                    None,
                    &[granted_s],
                )
                .await
                .expect("bounded thread starts")
            }
        });
    let entry =
        build_codex_thread_entry("t", worktree.path(), &start, None, None, true, None, None);
    // The request says full access...
    assert_eq!(entry.sandbox_posture.as_deref(), Some("danger-full-access"));
    // ...and the record says what actually came back, explicitly.
    assert_eq!(
        entry.resolved_sandbox.as_deref(),
        Some(crate::codex_thread::SANDBOX_POSTURE_UNKNOWN)
    );
    // The roots the thread carries onto every turn survive onto the row.
    assert!(
        entry.granted_writable_roots.contains(&granted_s),
        "granted roots {:?} must name {granted_s}",
        entry.granted_writable_roots
    );
}

#[test]
fn build_codex_thread_entry_stamps_the_request_node() {
    let worktree = tempfile::tempdir().unwrap();
    let _guard = crate::path_test_guard();
    let start = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let _daemon = crate::codex_fake_daemon::FakeDaemon::start(
                crate::codex_fake_daemon::Behavior::quick().with_thread_id("thread-node"),
            );
            crate::codex_thread::CodexThread::start(worktree.path(), None, true, None)
                .await
                .expect("yolo thread starts")
        });
    let entry = build_codex_thread_entry(
        "t",
        worktree.path(),
        &start,
        None,
        None,
        true,
        Some("x-535c"),
        None,
    );
    assert_eq!(entry.node.as_deref(), Some("x-535c"));
}

#[test]
fn build_codex_thread_entry_stamps_the_requested_account_verbatim() {
    // x-90a9 task 2.1: a pinned account record id survives onto the row so a
    // worker's account stays auditable; unpinned requests keep "default".
    let worktree = tempfile::tempdir().unwrap();
    let _guard = crate::path_test_guard();
    let start = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let _daemon = crate::codex_fake_daemon::FakeDaemon::start(
                crate::codex_fake_daemon::Behavior::quick().with_thread_id("thread-acct"),
            );
            crate::codex_thread::CodexThread::start(worktree.path(), None, true, None)
                .await
                .expect("yolo thread starts")
        });
    let pinned = build_codex_thread_entry(
        "t",
        worktree.path(),
        &start,
        None,
        None,
        true,
        None,
        Some("codex-main"),
    );
    assert_eq!(pinned.account_record_id.as_deref(), Some("codex-main"));
    let unpinned =
        build_codex_thread_entry("t", worktree.path(), &start, None, None, true, None, None);
    assert_eq!(unpinned.account_record_id.as_deref(), Some("default"));
    let blank = build_codex_thread_entry(
        "t",
        worktree.path(),
        &start,
        None,
        None,
        true,
        None,
        Some("   "),
    );
    assert_eq!(blank.account_record_id.as_deref(), Some("default"));
}

/// AC16: a codex PANE row (mux ref set) must refuse from the ask lane
/// naming the pane verb, never reach ensure_codex_thread_handle and die
/// with the confusing "is not a Codex thread".
#[tokio::test(flavor = "current_thread")]
async fn ask_a_codex_pane_row_refuses_naming_the_pane_verb() {
    let home = tmp_home("codex-pane-ask");
    state::update_registry(&home.registry_json(), |registry| {
        let mut entry = thread_entry("t-pane", AgentStatus::Live, None);
        entry.mux = Some(state::MuxRef {
            session: "main".into(),
            pane_id: 3,
        });
        entry.log_path = Some("/tmp/t-pane.log".into());
        registry.entries.push(entry);
    })
    .unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent"));
    let resp = handle_ask(
        &ctx,
        &Request::new(1, "agent.ask", json!({"name": "t-pane", "message": "hi"})),
    )
    .await;
    match &resp.payload {
        crate::protocol::ResponsePayload::Err(e) => {
            assert_eq!(e.code, ErrorCode::InvalidStatus);
            assert!(
                e.message.contains("pane worker") && e.message.contains("mux pane send"),
                "refusal must name the pane verb: {}",
                e.message
            );
            assert!(
                !e.message.contains("is not a Codex thread"),
                "the confusing thread refusal must not surface: {}",
                e.message
            );
        }
        _ => panic!("a pane row must refuse, got: {resp:?}"),
    }
    std::fs::remove_dir_all(home.root()).ok();
}

/// A resume writes what it actually resolved onto the row, not a stale
/// echo of what the ORIGINAL spawn recorded. Caught in review: the schema
/// this node adds (`resolved_sandbox`/`granted_writable_roots`) was wired
/// on the spawn path only, leaving exactly the gap the pre-existing
/// comment on `ensure_codex_thread_handle` named as its own durable fix -
/// "the sibling node that owns that schema" is this one.
///
/// `resume()` carries no state_dirs, so `granted_writable_roots` goes
/// empty here even though the row was seeded non-empty: that emptiness IS
/// the loss `codex_thread_resumed_without_state_grant` announces, not a
/// missed write. A row still showing the spawn-time roots after a resume
/// would look granted while actually ungranted.
#[tokio::test(flavor = "current_thread")]
async fn ensure_codex_thread_handle_records_what_the_resume_resolved() {
    // CODEX_HOME is process-global: hold the same guard every other fake
    // user holds, or a parallel test's driver reads THIS test's fake.
    let _guard = crate::path_test_guard();
    let home = tmp_home("codex-resume-records-posture");
    let cwd = tempfile::tempdir().unwrap();
    let _daemon = crate::codex_fake_daemon::FakeDaemon::start(
        crate::codex_fake_daemon::Behavior::quick().with_thread_id("thread-resumed"),
    );
    // Must match the fake's configured thread_id: `resume()` refuses when
    // `thread/resume` confirms a different id than requested.
    let session_id = "thread-resumed".to_string();
    state::update_registry(&home.registry_json(), |registry| {
        let mut entry = thread_entry("t-resume", AgentStatus::Live, None);
        entry.cwd = cwd.path().to_string_lossy().into_owned();
        entry.project_root = entry.cwd.clone();
        entry.harness_session_id = Some(session_id.clone());
        entry.codex_session_id = Some(session_id.clone());
        // Seeded as if a prior spawn had granted a root - the exact value
        // a stale write would leave behind uncorrected.
        entry.resolved_sandbox = Some("workspaceWrite".into());
        entry.granted_writable_roots = vec!["/stale/spawn-time/root".into()];
        registry.entries.push(entry);
    })
    .unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent"));
    let entry = state::load_registry(&home.registry_json())
        .unwrap()
        .find("t-resume")
        .cloned()
        .unwrap();
    ensure_codex_thread_handle(&ctx, &entry)
        .await
        .expect("the fake daemon answers thread/resume");

    let after = state::load_registry(&home.registry_json())
        .unwrap()
        .find("t-resume")
        .cloned()
        .unwrap();
    // The fake models no sandbox, so the fresh read is the explicit
    // unknown - not the workspaceWrite the row was seeded with.
    assert_eq!(
        after.resolved_sandbox.as_deref(),
        Some(crate::codex_thread::SANDBOX_POSTURE_UNKNOWN)
    );
    assert!(
        after.granted_writable_roots.is_empty(),
        "a resume carries no state_dirs, so the stale spawn-time root must \
         not survive: {:?}",
        after.granted_writable_roots
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test(flavor = "current_thread")]
async fn recovery_stamps_a_failed_codex_thread_resume_orphaned() {
    let home = tmp_home("codex-recover-orphaned");
    state::update_registry(&home.registry_json(), |registry| {
        let mut entry = thread_entry(
            "t-dead",
            AgentStatus::Live,
            Some("/tmp/t-dead.jsonl".into()),
        );
        entry.cwd = "/nonexistent-cwd-for-resume-failure".into();
        entry.project_root = entry.cwd.clone();
        entry.harness_session_id = Some("0198dead-0000-7000-8000-00000000000f".into());
        entry.codex_session_id = entry.harness_session_id.clone();
        entry.pid = None;
        registry.entries.push(entry);
    })
    .unwrap();
    let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
    recover_codex_threads(&ctx).await;
    let registry = load_registry_offloaded(home.registry_json())
        .await
        .expect("registry readable");
    assert_eq!(
        registry.find("t-dead").map(|entry| entry.status),
        Some(AgentStatus::Orphaned),
        "a failed resume must settle the row Orphaned, not Live-forever"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// The attach lane WITHOUT a harness-owned server (claude) must refuse
/// with the client-side-lane pointer, never reach the codex app-server
/// lane: `thread_lane` answers "attach" for claude too, so a bare lane
/// test would hand a claude thread spawn to codex's app-server.
#[tokio::test(flavor = "current_thread")]
async fn handle_spawn_thread_attach_without_server_refuses_with_client_pointer() {
    let home = tmp_home("spawn-thread-attach-client");
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(
        1,
        "agent.spawn",
        json!({"name": "test-agent", "provider": "claude", "substrate": "thread"}),
    );
    let resp = handle_spawn(&ctx, &req).await;
    match &resp.payload {
        crate::protocol::ResponsePayload::Err(e) => {
            assert_eq!(e.code, ErrorCode::InvalidParams);
            assert!(
                e.message.contains("--substrate thread"),
                "attach-without-server must point at the client-side lane; got: {}",
                e.message
            );
            assert!(
                !e.message.contains("retired at G4"),
                "this refusal is a lane split, not PTY retirement; got: {}",
                e.message
            );
        }
        _ => panic!("expected refusal for an attach lane without a server"),
    }
    std::fs::remove_dir_all(home.root()).ok();
}

/// A keeper-lane harness (agy) refuses naming fno's keeper process; the
/// text carries no mux pointer and no daemon-PTY retirement claim.
#[tokio::test(flavor = "current_thread")]
async fn handle_spawn_thread_keeper_lane_refuses_naming_keeper() {
    let home = tmp_home("spawn-thread-keeper");
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(
        1,
        "agent.spawn",
        json!({"name": "test-agent", "provider": "agy", "substrate": "thread"}),
    );
    let resp = handle_spawn(&ctx, &req).await;
    match &resp.payload {
        crate::protocol::ResponsePayload::Err(e) => {
            assert_eq!(e.code, ErrorCode::InvalidParams);
            assert!(
                e.message.contains("keeper"),
                "keeper-lane refusal must name the keeper process; got: {}",
                e.message
            );
            assert!(
                !e.message.contains("mux"),
                "keeper-lane refusal is not a PTY-retirement pointer; got: {}",
                e.message
            );
            assert!(
                !e.message.contains("retired at G4"),
                "keeper-lane refusal must not recycle the G4 message; got: {}",
                e.message
            );
        }
        _ => panic!("expected refusal for a keeper-lane harness"),
    }
    std::fs::remove_dir_all(home.root()).ok();
}

/// A permission axis the lane cannot resolve is refused at the RPC
/// boundary, before any thread starts. The alternative is a spawn that
/// succeeds at a posture nobody asked for, which reads as a working worker
/// until it cannot write.
#[tokio::test(flavor = "current_thread")]
async fn handle_spawn_thread_refuses_an_unmappable_permission_mode() {
    let home = tmp_home("spawn-thread-permission-mode");
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(
        1,
        "agent.spawn",
        json!({
            "name": "test-agent",
            "provider": "codex",
            "substrate": "thread",
            "permission_mode": "acceptEdits",
        }),
    );
    let resp = handle_spawn(&ctx, &req).await;
    match &resp.payload {
        crate::protocol::ResponsePayload::Err(e) => {
            assert!(
                e.message.contains("acceptEdits"),
                "refusal must name the value it could not map; got: {}",
                e.message
            );
        }
        _ => panic!("expected refusal for an unmappable permission_mode"),
    }
    std::fs::remove_dir_all(home.root()).ok();
}

/// An unknown harness on the thread substrate refuses via the contract
/// error rather than routing to any lane.
#[tokio::test(flavor = "current_thread")]
async fn handle_spawn_thread_unknown_harness_refuses() {
    let home = tmp_home("spawn-thread-unknown");
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(
        1,
        "agent.spawn",
        json!({"name": "test-agent", "provider": "nonexistent-provider", "substrate": "thread"}),
    );
    let resp = handle_spawn(&ctx, &req).await;
    match &resp.payload {
        crate::protocol::ResponsePayload::Err(e) => {
            assert_eq!(
                e.code,
                ErrorCode::InvalidParams,
                "unknown harness on the thread substrate must refuse"
            );
            assert!(
                e.message.contains("unknown harness"),
                "refusal must carry the contract error; got: {}",
                e.message
            );
        }
        _ => panic!("expected refusal for an unknown harness"),
    }
    std::fs::remove_dir_all(home.root()).ok();
}

/// The destination's own precondition: a provider the codex lane cannot
/// serve refuses loudly instead of silently starting a codex thread under
/// the caller's name. Pinned by calling the lane directly, because no
/// packaged row today answers attach-with-server except codex - this is
/// the guard a SECOND such row meets until its destination is wired.
#[tokio::test(flavor = "current_thread")]
async fn codex_thread_lane_refuses_a_provider_it_cannot_serve() {
    let home = tmp_home("codex-lane-wrong-provider");
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let req = Request::new(
        1,
        "agent.spawn",
        json!({"name": "test-agent", "provider": "claude", "substrate": "thread"}),
    );
    let resp = spawn_codex_thread_lane(&ctx, &req, "test-agent", Path::new("/tmp"), "claude").await;
    match &resp.payload {
        crate::protocol::ResponsePayload::Err(e) => {
            assert_eq!(e.code, ErrorCode::InvalidParams);
            assert!(
                e.message.contains("needs its own thread destination"),
                "the wrong-harness guard must name the missing destination; got: {}",
                e.message
            );
        }
        _ => panic!("expected the codex lane to refuse a provider it cannot serve"),
    }
    std::fs::remove_dir_all(home.root()).ok();
}

/// The thread route's provider default is `codex`, matching the client's
/// daemon-bound predicate: a thread spawn with no provider reaches the
/// app-server lane, never a refusal (green gate, mute worker - the
/// defaults-must-match note in client.rs run()).
#[tokio::test(flavor = "current_thread")]
async fn handle_spawn_thread_absent_provider_defaults_to_codex_lane() {
    with_fake_codex_daemon(crate::codex_fake_daemon::Behavior::quick(), async {
        let home = tmp_home("spawn-thread-default-provider");
        let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
        let worktree = home.root().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        let req = Request::new(
            1,
            "agent.spawn",
            json!({
                "name": "t",
                "substrate": "thread",
                "cwd": worktree.to_string_lossy(),
                "message": "seed turn",
            }),
        );
        let resp = handle_spawn(&ctx, &req).await;
        assert!(
            resp.result().is_some(),
            "absent provider must default to codex and reach the thread lane: {resp:?}"
        );
        std::fs::remove_dir_all(home.root()).ok();
    })
    .await;
}

/// Spawn a codex thread worker through the real handle_spawn and return
/// its response.
async fn spawn_codex_thread_for_test(ctx: &Ctx, home: &AgentsHome, seed: &str) -> Response {
    let worktree = home.root().join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let req = Request::new(
        1,
        "agent.spawn",
        json!({
            "name": "t",
            "provider": "codex",
            "substrate": "thread",
            "cwd": worktree.to_string_lossy(),
            "message": seed,
        }),
    );
    handle_spawn(ctx, &req).await
}

/// AC4 make-it-fail probe: an ask arriving while the SEED turn is driving
/// STEERS into it. Old mutex shape: the ask queued behind the whole seed
/// turn and drove a SECOND turn - this asserted reply would read REPLY-2
/// and two agent_ask_done events would land. Actor: one shared turn, one
/// event, the ask returns the seed turn's own reply.
#[tokio::test(flavor = "current_thread")]
async fn codex_thread_ask_while_driving_steers_instead_of_queueing() {
    with_fake_codex_daemon(crate::codex_fake_daemon::Behavior::quick(), async {
        let home = tmp_home("codex-steer");
        let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
        let spawned = spawn_codex_thread_for_test(&ctx, &home, "seed turn").await;
        assert!(spawned.result().is_some(), "spawn failed: {spawned:?}");

        let ask = handle_ask(
            &ctx,
            &Request::new(2, "agent.ask", json!({"name": "t", "message": "follow-up"})),
        )
        .await;
        let res = ask.result().expect("ask errored");
        assert_eq!(
            res["reply"], "REPLY-1",
            "the follow-up must ride the seed turn, not drive a second one: {res:?}"
        );

        // Exactly ONE completed turn: the seed and the steered ask share
        // it, so exactly one agent_ask_done event fires.
        let events = await_ask_done(&home).await;
        let done = events
            .iter()
            .filter(|e| e["type"] == "agent_ask_done")
            .count();
        assert_eq!(done, 1, "one shared turn must emit one event: {events:?}");
        ctx.codex_threads.lock().await.remove("t");
        std::fs::remove_dir_all(home.root()).ok();
    })
    .await;
}

/// AC10 (x-296f): a SEEDLESS codex thread spawn takes the warmup turn, so
/// a rollout exists and the worker is attachable from its first seconds.
/// The positive marker is the fake daemon's own received frame: a
/// `turn/start` carrying the warmup text. `thread/start` alone writes no
/// rollout and a harness resolves a session BY that rollout, so without
/// the warmup the first attach dies with "no rollout found for thread id".
#[tokio::test(flavor = "current_thread")]
async fn a_seedless_codex_thread_spawn_takes_the_warmup_turn() {
    let behavior = crate::codex_fake_daemon::Behavior::quick();
    let received = std::sync::Arc::clone(&behavior.received);
    with_fake_codex_daemon(behavior, async {
        let home = tmp_home("codex-warmup");
        let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
        // Seedless: the spawn request carries no message at all.
        let spawned = spawn_codex_thread_for_test(&ctx, &home, "").await;
        assert!(spawned.result().is_some(), "spawn failed: {spawned:?}");

        // The seed submit is async in the actor; wait for the frame rather
        // than racing it.
        let turns: Vec<serde_json::Value> = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let turns: Vec<serde_json::Value> = received
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .iter()
                    .filter(|f| f["method"] == "turn/start")
                    .cloned()
                    .collect();
                if !turns.is_empty() || std::time::Instant::now() >= deadline {
                    break turns;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        };
        assert_eq!(
            turns.len(),
            1,
            "a seedless spawn takes exactly one warmup turn: {turns:?}"
        );
        assert_eq!(
            turns[0]["params"]["input"][0]["text"], WARMUP_SEED,
            "the warmup is the seed that was submitted: {turns:?}"
        );

        ctx.codex_threads.lock().await.remove("t");
        std::fs::remove_dir_all(home.root()).ok();
    })
    .await;
}

/// The warmup must not double-submit behind a real seed: a spawn that
/// carries a prompt drives exactly that prompt, verbatim.
#[tokio::test(flavor = "current_thread")]
async fn a_seeded_codex_thread_spawn_drives_its_own_seed_only() {
    let behavior = crate::codex_fake_daemon::Behavior::quick();
    let received = std::sync::Arc::clone(&behavior.received);
    with_fake_codex_daemon(behavior, async {
        let home = tmp_home("codex-real-seed");
        let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
        let spawned = spawn_codex_thread_for_test(&ctx, &home, "do the actual work").await;
        assert!(spawned.result().is_some(), "spawn failed: {spawned:?}");

        let turns: Vec<serde_json::Value> = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let turns: Vec<serde_json::Value> = received
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .iter()
                    .filter(|f| f["method"] == "turn/start")
                    .cloned()
                    .collect();
                if !turns.is_empty() || std::time::Instant::now() >= deadline {
                    break turns;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        };
        assert_eq!(turns.len(), 1, "one seed, one turn: {turns:?}");
        assert_eq!(
            turns[0]["params"]["input"][0]["text"], "do the actual work",
            "a real seed passes through verbatim: {turns:?}"
        );

        ctx.codex_threads.lock().await.remove("t");
        std::fs::remove_dir_all(home.root()).ok();
    })
    .await;
}

/// AC5 + AC6 make-it-fail probe: stop INTERRUPTS the in-flight turn before
/// reporting stopped and names the interrupt outcome in the response. Old
/// shape: no `interrupt` key (remove-and-stamp while the turn task still
/// held an Arc clone), so the `interrupt == "interrupted"` assert fails
/// there.
///
/// It also pins the ownership claim this lane exists for. The row records
/// `pid: None` (it owns no process; the SHARED daemon, still running after
/// the stop, owns the thread). The assertion used to be the opposite (the
/// pid must be GONE), which is what owning a private app-server per worker
/// looked like.
#[tokio::test(flavor = "current_thread")]
async fn codex_thread_stop_interrupts_and_stamps_exited_without_killing_the_daemon() {
    with_fake_codex_daemon(crate::codex_fake_daemon::Behavior::long(), async {
        let home = tmp_home("codex-stop");
        let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
        let spawned = spawn_codex_thread_for_test(&ctx, &home, "long seed turn").await;
        assert!(spawned.result().is_some(), "spawn failed: {spawned:?}");
        let registry = load_registry_offloaded(home.registry_json())
            .await
            .expect("registry");
        assert_eq!(
            registry.find("t").and_then(|entry| entry.pid),
            None,
            "a thread row records no pid: it owns no process, and this \
             field is a liveness surface"
        );
        let daemon_state: Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::PathBuf::from(std::env::var("CODEX_HOME").unwrap())
                    .join("app-server-daemon")
                    .join("app-server.pid"),
            )
            .expect("daemon state"),
        )
        .expect("daemon state json");
        let pid = daemon_state["pid"].as_u64().expect("daemon pid") as u32;

        // Stop mid-turn, once the turn is actually driving.
        await_driving_turn(&ctx, "t").await;
        let stop = handle_stop(&ctx, &Request::new(3, "agent.stop", json!({"name": "t"}))).await;
        let res = stop.result().expect("stop errored");
        assert_eq!(res["stopped"], true, "stop response: {res:?}");
        assert_eq!(
            res["interrupt"], "interrupted",
            "stopped must name the interrupt outcome: {res:?}"
        );

        let registry = load_registry_offloaded(home.registry_json())
            .await
            .expect("registry");
        assert_eq!(
            registry.find("t").map(|e| e.status),
            Some(AgentStatus::Exited)
        );

        // The shared daemon must SURVIVE the stop. Stopping a worker
        // closes one connection; killing the app-server would take every
        // other codex session on the machine with it.
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        assert!(
            alive,
            "stopping a worker killed the shared app-server daemon {pid}"
        );
        ctx.codex_threads.lock().await.remove("t");
        std::fs::remove_dir_all(home.root()).ok();
    })
    .await;
}

/// The zombie-stop probe: an interrupt the daemon never confirms must NOT
/// report a stop.
///
/// With a private app-server, `kill_on_drop` made every stop terminal, so
/// `stopped: true` was always true. Against the shared daemon nothing ends
/// the turn but the interrupt itself, and an unconfirmed one leaves the
/// model taking that turn in the worker's worktree. Reporting `stopped:
/// true` there marks the row Exited, hides it from recovery, and discards
/// the interrupt handle, while the work continues unobserved.
///
/// The fake acks the interrupt and never completes the turn, which is
/// exactly that state.
#[tokio::test(flavor = "current_thread")]
async fn codex_thread_stop_refuses_over_a_turn_the_interrupt_never_settled() {
    let behavior = crate::codex_fake_daemon::Behavior::long().with_interrupt(
        crate::codex_fake_daemon::Interrupt::AckOnly(std::time::Duration::ZERO),
    );
    with_fake_codex_daemon(behavior, async {
        // A Drop guard, not a teardown line: an assertion below panics
        // out of this body, and a leaked bound would silently shorten
        // every later test's interrupt wait in the same process.
        struct BoundGuard;
        impl Drop for BoundGuard {
            fn drop(&mut self) {
                std::env::remove_var("FNO_CODEX_INTERRUPT_BOUND_MS");
            }
        }
        std::env::set_var("FNO_CODEX_INTERRUPT_BOUND_MS", "1500");
        let _bound = BoundGuard;
        let home = tmp_home("codex-zombie-stop");
        let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
        let spawned = spawn_codex_thread_for_test(&ctx, &home, "long seed turn").await;
        assert!(spawned.result().is_some(), "spawn failed: {spawned:?}");
        await_driving_turn(&ctx, "t").await;

        let stop = handle_stop(&ctx, &Request::new(3, "agent.stop", json!({"name": "t"}))).await;
        let res = stop.result().expect("stop errored");
        assert_eq!(
            res["stopped"], false,
            "an unsettled interrupt must not report a stop: {res:?}"
        );
        assert_eq!(
            res["interrupt"], "timeout-turn-still-running",
            "the response names why: {res:?}"
        );

        // The row stays non-terminal, so recovery can still see it, and
        // the handle stays so the live turn keeps an interrupt handle.
        let registry = load_registry_offloaded(home.registry_json())
            .await
            .expect("registry");
        assert_ne!(
            registry.find("t").map(|entry| entry.status),
            Some(AgentStatus::Exited),
            "a refused stop must not stamp the row terminal"
        );
        assert!(
            ctx.codex_threads.lock().await.contains_key("t"),
            "the actor must survive a refused stop; it holds the interrupt handle"
        );

        ctx.codex_threads.lock().await.remove("t");
        std::fs::remove_dir_all(home.root()).ok();
    })
    .await;
}

/// AC3 make-it-fail probe: an ask against a turn longer than the bounded
/// wait answers `in_flight` with the turn id while the turn keeps running.
/// Old shape: the ask blocked on the mutex for the whole 30s turn and
/// returned a completed reply - `status == "in_flight"` fails there.
#[tokio::test(flavor = "current_thread")]
async fn codex_thread_ask_returns_in_flight_when_turn_exceeds_bound() {
    with_fake_codex_daemon(crate::codex_fake_daemon::Behavior::long(), async {
        std::env::set_var("FNO_CODEX_ASK_WAIT_MS", "200");
        let home = tmp_home("codex-inflight");
        let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
        let spawned = spawn_codex_thread_for_test(&ctx, &home, "long seed turn").await;
        assert!(spawned.result().is_some(), "spawn failed: {spawned:?}");
        await_driving_turn(&ctx, "t").await;

        let started = std::time::Instant::now();
        let ask = handle_ask(
            &ctx,
            &Request::new(2, "agent.ask", json!({"name": "t", "message": "status?"})),
        )
        .await;
        let res = ask.result().expect("ask errored");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the bounded ask must answer near its 200ms bound, took {:?}",
            started.elapsed()
        );
        assert_eq!(res["status"], "in_flight", "in_flight receipt: {res:?}");
        assert!(res["reply"].is_null(), "in_flight reply is null: {res:?}");
        assert_eq!(
            res["turn_id"], "turn-1",
            "the receipt carries the surviving interrupt handle: {res:?}"
        );

        // Stop cleans up: interrupts the still-driving turn and kills it.
        let stop = handle_stop(&ctx, &Request::new(3, "agent.stop", json!({"name": "t"}))).await;
        let stop_res = stop.result().expect("stop errored");
        assert_eq!(stop_res["interrupt"], "interrupted");
        std::env::remove_var("FNO_CODEX_ASK_WAIT_MS");
        ctx.codex_threads.lock().await.remove("t");
        std::fs::remove_dir_all(home.root()).ok();
    })
    .await;
}

/// AC8 make-it-fail probe: mail arriving MID-TURN answers delivered on the
/// STEER ACK (milliseconds) and drives exactly ONE shared turn. Old shape:
/// the thread fell out of the switchboard as not-a-live-stream-thread, so
/// `delivered` read false - this assert fails there.
#[tokio::test(flavor = "current_thread")]
async fn switchboard_to_codex_thread_delivers_on_steering_ack_mid_turn() {
    with_fake_codex_daemon(crate::codex_fake_daemon::Behavior::quick(), async {
        let home = tmp_home("codex-mail");
        let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
        let spawned = spawn_codex_thread_for_test(&ctx, &home, "seed turn").await;
        assert!(spawned.result().is_some(), "spawn failed: {spawned:?}");
        let registry = load_registry_offloaded(home.registry_json())
            .await
            .expect("registry");
        let row = registry.find("t").expect("thread row").clone();
        await_driving_turn(&ctx, "t").await;

        let params = json!({
            "to": "t",
            "from": "king",
            "body": "hello thread",
            "mirror": false,
            "recipient_identity": {
                "harness": "codex",
                "session_id": row.harness_session_id,
                "short_id": "",
                "created_at": row.created_at,
            },
        });
        let started = std::time::Instant::now();
        let resp = handle_switchboard(&ctx, &Request::new(4, "agent.switchboard_v2", params)).await;
        let res = resp.result().expect("switchboard errored");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "delivery must answer on the steer ack, took {:?}",
            started.elapsed()
        );
        assert_eq!(res["delivered"], true, "codex mail: {res:?}");
        assert_eq!(res["identity_verified"], true);
        assert_eq!(res["turn_id"], "turn-1", "steered into the shared turn");

        // The body reached the thread: the steered turn carries it, so the
        // completion event names the same single turn.
        let events = await_ask_done(&home).await;
        let done: Vec<_> = events
            .iter()
            .filter(|e| e["type"] == "agent_ask_done")
            .collect();
        assert_eq!(done.len(), 1, "one shared turn: {events:?}");
        assert_eq!(
            done[0]["data"]["turn_id"], "turn-1",
            "the completion must name the turn both submits shared: {events:?}"
        );
        let injected = events.iter().any(|e| {
            e["type"] == "agent_deliver_injected"
                && e["data"]["transport"] == "switchboard"
                && e["data"]["provider"] == "codex"
        });
        assert!(injected, "injected event missing: {events:?}");

        // Cleanup: the actor holds a live daemon connection.
        handle_stop(&ctx, &Request::new(5, "agent.stop", json!({"name": "t"}))).await;
        ctx.codex_threads.lock().await.remove("t");
        std::fs::remove_dir_all(home.root()).ok();
    })
    .await;
}

/// AC8 (idle half): mail to an IDLE codex thread starts the turn itself
/// and answers delivered with that turn id - no pane, no durable demote.
#[tokio::test(flavor = "current_thread")]
async fn switchboard_to_idle_codex_thread_delivers_on_start_ack() {
    with_fake_codex_daemon(crate::codex_fake_daemon::Behavior::quick(), async {
        let home = tmp_home("codex-mail-idle");
        let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
        // No seed: the row is idle at mail time.
        let spawned = spawn_codex_thread_for_test(&ctx, &home, "").await;
        assert!(spawned.result().is_some(), "spawn failed: {spawned:?}");
        let registry = load_registry_offloaded(home.registry_json())
            .await
            .expect("registry");
        let row = registry.find("t").expect("thread row").clone();

        let params = json!({
            "to": "t",
            "from": "king",
            "body": "wake up",
            "mirror": false,
            "recipient_identity": {
                "harness": "codex",
                "session_id": row.harness_session_id,
                "short_id": "",
                "created_at": row.created_at,
            },
        });
        let resp = handle_switchboard(&ctx, &Request::new(4, "agent.switchboard_v2", params)).await;
        let res = resp.result().expect("switchboard errored");
        assert_eq!(res["delivered"], true, "idle codex mail: {res:?}");
        assert_eq!(res["turn_id"], "turn-1", "started the turn: {res:?}");

        handle_stop(&ctx, &Request::new(5, "agent.stop", json!({"name": "t"}))).await;
        ctx.codex_threads.lock().await.remove("t");
        std::fs::remove_dir_all(home.root()).ok();
    })
    .await;
}

/// x-fd66: a thread row's status comes from its driver, not from a pane that
/// is not there. A codex thread worker mid-turn reads `working` (ttl set,
/// fresh stamp) with no pane attached at any point, then `done` (no ttl) once
/// the completion routes - through the same seq-gated writer the claude
/// inside-leg hook uses. Asserted against the row's stored report, the
/// driver's own turn state, never a rendered glyph.
#[tokio::test(flavor = "current_thread")]
async fn codex_thread_row_reports_working_then_done_with_no_pane() {
    with_fake_codex_daemon(crate::codex_fake_daemon::Behavior::quick(), async {
        let home = tmp_home("codex-inside-leg-e2e");
        let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
        let spawned = spawn_codex_thread_for_test(&ctx, &home, "seed turn").await;
        assert!(spawned.result().is_some(), "spawn failed: {spawned:?}");
        await_driving_turn(&ctx, "t").await;

        // Mid-turn: Working, ttl set, seq allocated. The write is offloaded,
        // so poll for the marker instead of betting on a fixed sleep.
        let working = poll_thread_row(&home.registry_json(), |report| {
            report.state == state::InsideLegState::Working
        })
        .await;
        assert_eq!(working.ttl_ms, Some(state::THREAD_TURN_TTL_MS));
        let working_seq = working.seq;
        assert!(working_seq >= 1);

        // After the completion routes: Done, no ttl, seq above the working one.
        await_ask_done(&home).await;
        let done = poll_thread_row(&home.registry_json(), |report| {
            report.state == state::InsideLegState::Done
        })
        .await;
        assert_eq!(done.ttl_ms, None, "a done report never ages out");
        assert!(
            done.seq > working_seq,
            "done seq {} must clear above working seq {working_seq}",
            done.seq
        );

        ctx.codex_threads.lock().await.remove("t");
        std::fs::remove_dir_all(home.root()).ok();
    })
    .await;
}

/// The refresh half (x-fd66): while the driver keeps answering, the working
/// report is rewritten at the keepalive cadence, so a turn longer than the
/// report ttl never ages to `?` mid-flight. The fake's 30s turn against a
/// 100ms refresh proves the seq advances with no completion in between; the
/// interrupt then ends the turn and the row settles Done.
#[tokio::test(flavor = "current_thread")]
async fn codex_thread_working_report_refreshes_while_the_turn_drives() {
    struct RefreshGuard;
    impl Drop for RefreshGuard {
        fn drop(&mut self) {
            std::env::remove_var("FNO_THREAD_TURN_REFRESH_MS");
        }
    }
    with_fake_codex_daemon(crate::codex_fake_daemon::Behavior::long(), async {
        // Set INSIDE the guard: the var is process-wide and parallel tests'
        // actors read it at birth - a 100ms cadence leaking into another
        // test floods the offloaded registry-write queue and races the
        // stop path's status write.
        std::env::set_var("FNO_THREAD_TURN_REFRESH_MS", "100");
        let _refresh = RefreshGuard;
        let home = tmp_home("codex-inside-leg-refresh");
        let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
        let spawned = spawn_codex_thread_for_test(&ctx, &home, "long seed turn").await;
        assert!(spawned.result().is_some(), "spawn failed: {spawned:?}");
        await_driving_turn(&ctx, "t").await;

        // At least one REFRESH landed: seq advanced past the ack's write with
        // the same turn still driving (no completion ever routed).
        let refreshed = poll_thread_row(&home.registry_json(), |report| report.seq >= 3).await;
        assert_eq!(refreshed.state, state::InsideLegState::Working);
        assert_eq!(refreshed.ttl_ms, Some(state::THREAD_TURN_TTL_MS));

        // Ending the turn routes the completion; the row settles Done.
        let stop = handle_stop(&ctx, &Request::new(3, "agent.stop", json!({"name": "t"}))).await;
        assert_eq!(
            stop.result().expect("stop errored")["interrupt"],
            "interrupted"
        );
        let done = poll_thread_row(&home.registry_json(), |report| {
            report.state == state::InsideLegState::Done
        })
        .await;
        assert_eq!(done.ttl_ms, None);

        ctx.codex_threads.lock().await.remove("t");
        std::fs::remove_dir_all(home.root()).ok();
    })
    .await;
}

/// The resume half (x-fd66): the actor's report seq starts ABOVE the row's
/// current seq, so the resumed thread's first report clears the gate instead
/// of dying under the previous incarnation's seq. The row is seeded with a
/// done report at seq 7, as a prior life of this thread would have left it.
#[tokio::test(flavor = "current_thread")]
async fn codex_thread_resume_writes_above_the_row_seq() {
    // CODEX_HOME is process-global: hold the same guard every other fake
    // user holds, or a parallel test's driver reads THIS test's fake.
    let _guard = crate::path_test_guard();
    let _daemon = crate::codex_fake_daemon::FakeDaemon::start(
        crate::codex_fake_daemon::Behavior::quick().with_thread_id("thread-seq"),
    );
    let home = tmp_home("codex-inside-leg-resume-seq");
    state::update_registry(&home.registry_json(), |registry| {
        let mut entry = thread_entry("t-seq", AgentStatus::Live, None);
        let cwd = home.root().join("worktree");
        std::fs::create_dir_all(&cwd).unwrap();
        entry.cwd = cwd.to_string_lossy().into_owned();
        entry.project_root = entry.cwd.clone();
        entry.harness_session_id = Some("thread-seq".into());
        entry.codex_session_id = Some("thread-seq".into());
        entry.inside_leg = Some(state::InsideLegReport {
            state: state::InsideLegState::Done,
            seq: 7,
            reason: None,
            received_at: "2020-01-01T00:00:00Z".into(),
            ttl_ms: None,
        });
        registry.entries.push(entry);
    })
    .unwrap();
    let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
    let entry = state::load_registry(&home.registry_json())
        .unwrap()
        .find("t-seq")
        .cloned()
        .unwrap();
    ensure_codex_thread_handle(&ctx, &entry)
        .await
        .expect("the fake daemon answers thread/resume");

    // One completed turn: the seeded counter (8) is what the row accepts.
    let ask = handle_ask(
        &ctx,
        &Request::new(2, "agent.ask", json!({"name": "t-seq", "message": "hi"})),
    )
    .await;
    let res = ask.result().expect("ask errored");
    assert_eq!(
        res["reply"], "REPLY-1",
        "the resumed thread answers: {res:?}"
    );
    let done = poll_thread_row_named(&home.registry_json(), "t-seq", |report| {
        report.state == state::InsideLegState::Done
    })
    .await;
    assert!(
        done.seq >= 8,
        "the resumed thread's write must clear a row at seq 7, got seq {}",
        done.seq
    );

    ctx.codex_threads.lock().await.remove("t-seq");
    std::fs::remove_dir_all(home.root()).ok();
}

/// The episode gate (x-fd66 + x-dd84): notify intent fires on the EDGE into
/// done, once per episode, never on a repeat; a stale-seq write is dropped by
/// the same gate; the scrape verdict is cleared on the flip; a row holding no
/// such session is a no-op. The thread writer and the hook flush share this
/// core, so one test pins both.
#[test]
fn gate_inside_leg_onto_row_notifies_once_per_done_episode() {
    let mut registry = state::Registry::default();
    let mut row = thread_entry("t-gate", AgentStatus::Live, None);
    row.codex_session_id = Some("sid-gate".into());
    registry.entries.push(row);
    registry.entries[0].screen_state = Some(state::ScreenStateReport {
        state: "working".into(),
        rule: "busy".into(),
        seq: 1,
        at: "2020-01-01T00:00:00Z".into(),
        ttl_ms: None,
        answerable: None,
    });

    let rep = |seq, st| state::InsideLegReport {
        state: st,
        seq,
        reason: None,
        received_at: "2020-01-01T00:00:00Z".into(),
        ttl_ms: None,
    };

    // Working: accepted, no notify, scrape verdict cleared.
    let n = gate_inside_leg_onto_row(
        &mut registry,
        "sid-gate",
        rep(1, state::InsideLegState::Working),
    );
    assert_eq!(n, None, "working is not an episode edge");
    assert_eq!(registry.entries[0].inside_leg.as_ref().unwrap().seq, 1);
    assert!(
        registry.entries[0].screen_state.is_none(),
        "the flip clears the scrape verdict"
    );

    // Done: the episode edge, exactly one intent, and it names is_done.
    let n = gate_inside_leg_onto_row(
        &mut registry,
        "sid-gate",
        rep(2, state::InsideLegState::Done),
    );
    assert_eq!(n, Some(("done".to_string(), true)));

    // A repeat Done (seq 3): accepted by the seq gate but NOT a new episode.
    let n = gate_inside_leg_onto_row(
        &mut registry,
        "sid-gate",
        rep(3, state::InsideLegState::Done),
    );
    assert_eq!(n, None, "a repeat done must not re-fire the episode");
    assert_eq!(registry.entries[0].inside_leg.as_ref().unwrap().seq, 3);

    // A stale-seq write (seq 3 again): dropped entirely.
    let n = gate_inside_leg_onto_row(
        &mut registry,
        "sid-gate",
        rep(3, state::InsideLegState::Working),
    );
    assert_eq!(n, None, "seq <= current is dropped");
    assert_eq!(
        registry.entries[0].inside_leg.as_ref().unwrap().state,
        state::InsideLegState::Done
    );

    // A row holding no such session: no-op.
    let n = gate_inside_leg_onto_row(
        &mut registry,
        "sid-other",
        rep(9, state::InsideLegState::Done),
    );
    assert_eq!(n, None);
}

/// Poll until the row named `t` carries an inside-leg report matching
/// `pred`, and return it. Polling instead of a fixed sleep: the writer runs
/// off the actor task, so a sleep bets the write landed; this waits for the
/// marker itself.
async fn poll_thread_row_named(
    registry_path: &std::path::Path,
    name: &str,
    pred: impl Fn(&state::InsideLegReport) -> bool,
) -> state::InsideLegReport {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let report = load_registry_offloaded(registry_path.to_path_buf())
            .await
            .ok()
            .and_then(|registry| registry.find(name).cloned())
            .and_then(|entry| entry.inside_leg);
        if let Some(report) = report {
            if pred(&report) {
                return report;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no matching inside-leg report ever landed for {name}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// [`poll_thread_row_named`] for the default test row name `t`.
async fn poll_thread_row(
    registry_path: &std::path::Path,
    pred: impl Fn(&state::InsideLegReport) -> bool,
) -> state::InsideLegReport {
    poll_thread_row_named(registry_path, "t", pred).await
}
