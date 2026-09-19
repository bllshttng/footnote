//! The rm tests that drive `handle_rm_with` to a success response. Every
//! success tail calls `release_stopped_claims_into`, which resolves the
//! claims root from the process-global `FNO_CLAIMS_ROOT`, so each of these
//! holds `test_env_lock` for its whole body. Moved verbatim out of
//! daemon.rs for file budget: test motion is the sanctioned shrink.

use super::*;
#[tokio::test]
async fn rm_cascades_claude_before_removing_the_registry_row() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = short_home("rmclaude");
    let row = claude_rm_row(
        "stopped-worker",
        "aaaa1111",
        "aaaa1111-1111-2222-3333-444444444444",
    );
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "stopped-worker"}));
    let called = std::sync::Mutex::new(Vec::new());
    let snapshots = claude_row_then_absent("aaaa1111", "stopped");

    let response = handle_rm_with(
        &ctx,
        &request,
        &snapshots,
        &|short_id| {
            called.lock().unwrap().push(short_id.to_string());
            Ok(())
        },
        &|_| true,
        &|_, _| Ok(true),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert_eq!(called.into_inner().unwrap(), vec!["aaaa1111"]);
    assert_eq!(response.result().unwrap()["harness_removed"], true);
    assert!(state::load_registry(&home.registry_json())
        .unwrap()
        .entries
        .is_empty());
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn rm_cleans_a_crowned_rows_scope_manifest_best_effort() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = short_home("rmcrownstate");
    let project = home.root().join("project");
    // The manifest lives where the row's cwd resolves it: the project's
    // space dir under the pinned FNO_SPACES_DIR, not a repo-relative .fno.
    let spaces = home.root().join("spaces");
    std::env::set_var("FNO_SPACES_DIR", &spaces);
    let manifest = crate::paths::space_dir_opt(&project)
        .unwrap()
        .join("kings")
        .join("alpha.md");
    std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    std::fs::write(&manifest, "---\nscope: alpha\n---\n").unwrap();
    let mut row = claude_rm_row(
        "stopped-worker",
        "aaaa2222",
        "aaaa2222-1111-2222-3333-444444444444",
    );
    row.cwd = project.to_string_lossy().into_owned();
    row.crown_level = Some(1);
    row.crown_scope = Some("alpha".into());
    row.crown_grantor = Some("human".into());
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "stopped-worker"}));
    let snapshots = claude_row_then_absent("aaaa2222", "stopped");

    let response = handle_rm_with(
        &ctx,
        &request,
        &snapshots,
        &|_| Ok(()),
        &|_| true,
        &|_, _| Ok(true),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert!(response.error().is_none(), "{response:?}");
    assert!(
        !manifest.exists(),
        "successful rm left crown loop state behind"
    );
    std::env::remove_var("FNO_SPACES_DIR");
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn rm_never_deletes_a_successors_re_armed_manifest() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // The vacated row is removed with a manifest on disk naming a
    // DIFFERENT session: a successor crowned over the scope after this
    // row went terminal re-armed it. Deleting that file would disarm the
    // live successor's stop gate.
    let home = short_home("rmcrownsucc");
    let project = home.root().join("project");
    let spaces = home.root().join("spaces");
    std::env::set_var("FNO_SPACES_DIR", &spaces);
    let manifest = crate::paths::space_dir_opt(&project)
        .unwrap()
        .join("kings")
        .join("alpha.md");
    std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    std::fs::write(
        &manifest,
        "---\nscope: alpha\nharness_session_id: bbbb9999-9999-4999-8999-999999999999\n---\n",
    )
    .unwrap();
    let mut row = claude_rm_row(
        "stopped-worker",
        "aaaa3333",
        "aaaa3333-1111-2222-3333-444444444444",
    );
    row.cwd = project.to_string_lossy().into_owned();
    row.crown_level = Some(1);
    row.crown_scope = Some("alpha".into());
    row.crown_grantor = Some("human".into());
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "stopped-worker"}));
    let snapshots = claude_row_then_absent("aaaa3333", "stopped");

    let response = handle_rm_with(
        &ctx,
        &request,
        &snapshots,
        &|_| Ok(()),
        &|_| true,
        &|_, _| Ok(true),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert!(response.error().is_none(), "{response:?}");
    assert!(
        manifest.exists(),
        "rm deleted a manifest naming a different session: the live successor's gate"
    );
    std::env::remove_var("FNO_SPACES_DIR");
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn rm_removes_a_row_the_registry_actually_holds() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // AC1-HP: a plain removal reports removed:true and a re-read shows
    // zero rows for the name.
    let home = short_home("rmhappy");
    let row = ask_row("w1", Some("2020-01-01T00:00:00Z"));
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "w1"}));

    let response = handle_rm_with(
        &ctx,
        &request,
        &|| crate::claude_roster::ClaudeAgentsSnapshot::known(Vec::new()),
        &|_| Ok(()),
        &|_| true,
        &|_, _| Ok(true),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert_eq!(response.result().unwrap()["removed"], true);
    assert!(state::load_registry(&home.registry_json())
        .unwrap()
        .entries
        .is_empty());
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn rm_keeps_the_audit_event_compact_when_diagnostics_are_oversized() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = short_home("rmeventoversize");
    let row = claude_rm_row(
        "stopped-worker",
        "bbbb2223",
        "bbbb2223-1111-2222-3333-444444444444",
    );
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(
        1,
        "agent.rm",
        json!({"name": "stopped-worker", "force": true}),
    );

    let response = handle_rm_with(
        &ctx,
        &request,
        &|| {
            crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                crate::claude_roster::ClaudeAgentRow::new("bbbb2223", Some("stopped")),
            ])
        },
        &|_| Err("x".repeat(crate::events_limits::max_data_bytes() * 2)),
        &|_| true,
        &|_, _| Ok(true),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert_eq!(response.result().unwrap()["event_written"], true);
    assert!(response.result().unwrap()["event_reason"].is_null());
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn rm_falls_back_to_the_session_uuid_prefix() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = short_home("rmfallback");
    let row = claude_rm_row("stopped-worker", "", "cccc3333-1111-2222-3333-444444444444");
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "stopped-worker"}));
    let called = std::sync::Mutex::new(Vec::new());
    let snapshots = claude_row_then_absent("cccc3333", "stopped");

    let response = handle_rm_with(
        &ctx,
        &request,
        &snapshots,
        &|short_id| {
            called.lock().unwrap().push(short_id.to_string());
            Ok(())
        },
        &|_| true,
        &|_, _| Ok(true),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert!(response.result().is_some());
    assert_eq!(called.into_inner().unwrap(), vec!["cccc3333"]);
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn rm_accepts_terminal_claude_rows_that_remain_in_the_roster() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (index, state) in ["done", "stopped", "failed"].into_iter().enumerate() {
        let home = short_home(&format!("rmterminal{state}"));
        let short_id = format!("dead{index:04}");
        let session_id = format!("{short_id}-1111-2222-3333-444444444444");
        let mut row = claude_rm_row("finished-worker", &short_id, &session_id);
        row.status = AgentStatus::Live;
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "finished-worker"}));
        let first = std::sync::atomic::AtomicBool::new(true);

        let response = handle_rm_with(
            &ctx,
            &request,
            &|| {
                if first.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                        crate::claude_roster::ClaudeAgentRow::new(&short_id, Some(state)),
                    ])
                } else {
                    crate::claude_roster::ClaudeAgentsSnapshot::known(Vec::new())
                }
            },
            &|_| Ok(()),
            &|_| true,
            &|_, _| Ok(true),
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        assert_eq!(response.result().unwrap()["removed"], true, "state={state}");
        assert!(state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .is_empty());
        std::fs::remove_dir_all(home.root()).ok();
    }
}

#[tokio::test]
async fn rm_unknown_claude_list_cascades_but_reports_unverified() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = short_home("rmunverified");
    let row = claude_rm_row(
        "stopped-worker",
        "eeee5555",
        "eeee5555-1111-2222-3333-444444444444",
    );
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "stopped-worker"}));
    let called = std::sync::atomic::AtomicBool::new(false);

    let response = handle_rm_with(
        &ctx,
        &request,
        &|| crate::claude_roster::ClaudeAgentsSnapshot::unknown("list timed out"),
        &|_| {
            called.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        },
        &|_| true,
        &|_, _| Ok(true),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert!(called.load(std::sync::atomic::Ordering::Relaxed));
    assert!(response.result().unwrap()["harness_removed"].is_null());
    assert!(state::load_registry(&home.registry_json())
        .unwrap()
        .entries
        .is_empty());
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn rm_removes_a_stored_live_row_provably_gone_from_the_roster() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // AC2-HP (false-refusal mode): a row torn down by hand with `claude
    // stop`/`claude rm` never gets AgentStatus::Live written back. The
    // live gate must reconcile with the roster, not the stored enum.
    let home = short_home("rmprovengone");
    let mut row = claude_rm_row(
        "hand-torn-down",
        "ffff6666",
        "ffff6666-1111-2222-3333-444444444444",
    );
    row.status = AgentStatus::Live;
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "hand-torn-down"}));

    let response = handle_rm_with(
        &ctx,
        &request,
        &|| crate::claude_roster::ClaudeAgentsSnapshot::known(Vec::new()),
        &|_| panic!("row already absent from the roster must not reach claude rm"),
        &|_| true,
        &|_, _| Ok(true),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert_eq!(response.result().unwrap()["removed"], true);
    assert!(state::load_registry(&home.registry_json())
        .unwrap()
        .entries
        .is_empty());
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn rm_kills_a_mux_pane_before_removing_its_registry_row() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = short_home("rmpane");
    let mut row = ask_row("pane-worker", Some("2020-01-01T00:00:00Z"));
    row.harness = Some("gemini".into());
    row.mux = Some(state::MuxRef {
        session: "main".into(),
        pane_id: 24,
    });
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "pane-worker"}));
    let killed = std::sync::Mutex::new(Vec::new());

    let response = handle_rm_with(
        &ctx,
        &request,
        &|| panic!("non-Claude row must not read the Claude list"),
        &|_| panic!("non-Claude row must not call claude rm"),
        &|_| panic!("no claude stop may run in this test"),
        &|session, pane_id| {
            killed.lock().unwrap().push((session.to_string(), pane_id));
            Ok(true)
        },
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert_eq!(killed.into_inner().unwrap(), vec![("main".to_string(), 24)]);
    assert_eq!(response.result().unwrap()["pane_removed"], true);
    assert!(state::load_registry(&home.registry_json())
        .unwrap()
        .entries
        .is_empty());
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn rm_clears_a_stale_registry_row_after_the_mux_pane_is_already_absent() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = short_home("rmmissingpane");
    let mut row = ask_row("stale-pane-worker", Some("2020-01-01T00:00:00Z"));
    row.harness = Some("gemini".into());
    row.mux = Some(state::MuxRef {
        session: "main".into(),
        pane_id: 24,
    });
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "stale-pane-worker"}));

    let response = handle_rm_with(
        &ctx,
        &request,
        &|| panic!("non-Claude row must not read the Claude list"),
        &|_| panic!("non-Claude row must not call claude rm"),
        &|_| panic!("no claude stop may run in this test"),
        &|_, _| Ok(false),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert_eq!(response.result().unwrap()["pane_removed"], false);
    assert_eq!(
        response.result().unwrap()["pane_reason"],
        "mux pane already absent"
    );
    assert!(state::load_registry(&home.registry_json())
        .unwrap()
        .entries
        .is_empty());
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn rm_clears_a_stored_live_pane_row_whose_pane_is_provably_absent() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // The fleet-reap deadlock: the row still reads live while the pane it
    // names is gone. The gate must test the referent, so the probe's
    // Absent verdict clears the row without --force.
    let home = short_home("rmpanegone");
    let mut row = ask_row("dead-pane-worker", Some("2020-01-01T00:00:00Z"));
    row.harness = Some("opencode".into());
    row.status = AgentStatus::Live;
    row.mux = Some(state::MuxRef {
        session: "main".into(),
        pane_id: 76,
    });
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "dead-pane-worker"}));
    let probed = std::sync::Mutex::new(Vec::new());

    let response = handle_rm_with(
        &ctx,
        &request,
        &|| panic!("non-Claude row must not read the Claude list"),
        &|_| panic!("non-Claude row must not call claude rm"),
        &|_| panic!("no claude stop may run in this test"),
        &|session, pane_id| {
            assert_eq!((session, pane_id), ("main", 76));
            Ok(false)
        },
        &|session, pane_id| {
            probed.lock().unwrap().push((session.to_string(), pane_id));
            PaneProbe::Absent
        },
    )
    .await;

    assert_eq!(probed.into_inner().unwrap(), vec![("main".to_string(), 76)]);
    assert_eq!(response.result().unwrap()["removed"], true);
    assert_eq!(response.result().unwrap()["pane_removed"], false);
    assert_eq!(
        response.result().unwrap()["pane_reason"],
        "mux pane already absent"
    );
    assert!(state::load_registry(&home.registry_json())
        .unwrap()
        .entries
        .is_empty());
    std::fs::remove_dir_all(home.root()).ok();
}

/// AC4-HP: a live claude background thread is ended by rm's OWN claude stop
/// (law d-81c6da7e), which runs BEFORE the harness cascade's claude rm, and
/// the row is removed. The post-stop roster read shows the session in a
/// terminal state, so the gate clears and the cascade still reaches
/// `claude rm` (the row is present, just finished).
#[tokio::test]
async fn rm_stops_the_claude_thread_itself_before_the_cascade() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = short_home("rmselfstop");
    let mut row = claude_rm_row(
        "bg-thread",
        "eeee7777",
        "eeee7777-1111-2222-3333-444444444444",
    );
    row.status = AgentStatus::Live;
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "bg-thread"}));
    let order = std::sync::Mutex::new(Vec::new());
    let first = std::sync::atomic::AtomicBool::new(true);
    let second = std::sync::atomic::AtomicBool::new(true);

    let response = handle_rm_with(
        &ctx,
        &request,
        &|| {
            if first.swap(false, std::sync::atomic::Ordering::Relaxed) {
                crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                    crate::claude_roster::ClaudeAgentRow::new("eeee7777", Some("running")),
                ])
            } else if second.swap(false, std::sync::atomic::Ordering::Relaxed) {
                // The post-stop re-read: the stop worked, the session reads
                // finished, and the row is still listed (claude keeps
                // finished agents).
                crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                    crate::claude_roster::ClaudeAgentRow::new("eeee7777", Some("stopped")),
                ])
            } else {
                // The cascade's post-removal read: the harness row is gone.
                crate::claude_roster::ClaudeAgentsSnapshot::known(Vec::new())
            }
        },
        &|short_id| {
            order.lock().unwrap().push(format!("rm {short_id}"));
            Ok(())
        },
        &|_| {
            order.lock().unwrap().push("stop".to_string());
            true
        },
        &|_, _| panic!("a background thread has no pane"),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert_eq!(
        order.into_inner().unwrap(),
        vec!["stop".to_string(), "rm eeee7777".to_string()]
    );
    assert_eq!(response.result().unwrap()["removed"], true);
    assert!(state::load_registry(&home.registry_json())
        .unwrap()
        .entries
        .is_empty());
    std::fs::remove_dir_all(home.root()).ok();
}

/// AC3-HP: a live pane-substrate row with a verifiable pid is ended by rm's
/// own pane stop (the pid-proving helper), and the row is removed. The
/// child is a real process so the precheck's ownership probe and the ESRCH
/// confirmation are the production ones.
#[cfg(unix)]
#[tokio::test]
async fn rm_ends_a_live_pane_row_through_its_own_pane_stop() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = short_home("rmpanestop");
    let mut row = ask_row("pane-row", Some("2020-01-01T00:00:00Z"));
    row.status = AgentStatus::Live;
    row.substrate = Some("pane".into());
    row.harness = Some("claude".into());
    // The sleeper must NOT be this test's own child: an unreaped zombie
    // still answers kill(0), so the ESRCH confirmation would never fire.
    // A shell-detached sleeper is reparented, and its death reads as gone.
    let sh = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("sleep 30 >/dev/null 2>&1 & echo $!")
        .output()
        .expect("test spawns a detached sleeper");
    let pid: u32 = String::from_utf8_lossy(&sh.stdout)
        .trim()
        .parse()
        .expect("the shell echoes the sleeper pid");
    row.pid = Some(pid);
    row.pid_start_time = crate::daemon::process_start_time(pid);
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "pane-row"}));

    let response = handle_rm_with(
        &ctx,
        &request,
        &|| crate::claude_roster::ClaudeAgentsSnapshot::known(Vec::new()),
        &|_| panic!("rm must not reach claude rm for a pane row"),
        &|_| panic!("no claude stop may run for a pane-substrate row"),
        &|_, _| panic!("a pane-substrate row takes the pane arm, not the mux kill"),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert!(
        response.error().is_none(),
        "pane rm refused: {:?}",
        response.error().map(|e| e.message.clone())
    );
    assert_eq!(response.result().unwrap()["removed"], true);
    assert!(state::load_registry(&home.registry_json())
        .unwrap()
        .entries
        .is_empty());
    std::fs::remove_dir_all(home.root()).ok();
}

/// AC2-HP: a live codex thread row with a hosted actor is removed by rm
/// alone: the teardown interrupts and drops the actor (so the map empties)
/// and no stop verb runs. The claude seams panic - a codex row must never
/// reach them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rm_removes_a_hosted_codex_thread_and_drops_its_actor() {
    let _guard = crate::path_test_guard();
    let _daemon = crate::codex_fake_daemon::FakeDaemon::start(
        crate::codex_fake_daemon::Behavior::quick().with_thread_id("thread-rm-actor"),
    );
    let home = short_home("rmthreadactor");
    let cwd = tempfile::tempdir().unwrap();
    let mut row = thread_entry("t-rm-actor", AgentStatus::Live, None);
    row.cwd = cwd.path().to_string_lossy().into_owned();
    row.project_root = row.cwd.clone();
    row.harness_session_id = Some("thread-rm-actor".into());
    row.codex_session_id = Some("thread-rm-actor".into());
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent"));
    let entry = state::load_registry(&home.registry_json())
        .unwrap()
        .find("t-rm-actor")
        .cloned()
        .unwrap();
    ensure_codex_thread_handle(&ctx, &entry)
        .await
        .expect("the fake daemon answers thread/resume");
    let request = Request::new(1, "agent.rm", json!({"name": "t-rm-actor"}));

    let response = handle_rm_with(
        &ctx,
        &request,
        &|| panic!("a codex row must not read the claude roster"),
        &|_| panic!("rm must not reach claude rm"),
        &|_| panic!("no claude stop may run for a codex row"),
        &|_, _| panic!("a thread row has no mux ref to kill"),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert!(
        response.error().is_none(),
        "{:?}",
        response.error().map(|e| e.message.clone())
    );
    assert_eq!(response.result().unwrap()["removed"], true);
    assert!(state::load_registry(&home.registry_json())
        .unwrap()
        .entries
        .is_empty());
    assert!(ctx.codex_threads.lock().await.is_empty());
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn rm_ends_a_live_non_thread_codex_row_with_no_stop_leg() {
    // Law d-81c6da7e: rm ends a live codex row itself (the widened
    // worker-stop arm; this row is not a thread entry, so the worker
    // socket decides). No roster read, no claude rm, no stop leg.
    let home = short_home("rmorphancodex");
    let mut row = ask_row("live-codex", Some("2020-01-01T00:00:00Z"));
    row.harness = Some("codex".into());
    row.status = AgentStatus::Live;
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "live-codex"}));

    let response = handle_rm_with(
        &ctx,
        &request,
        &|| panic!("a non-claude row never reads the claude roster"),
        &|_| panic!("a live row must not reach claude rm"),
        &|_| panic!("no claude stop may run for a codex row"),
        &|_, _| panic!("a mux-less codex row has no pane to kill"),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert!(
        response.error().is_none(),
        "{:?}",
        response.error().map(|e| e.message.clone())
    );
    assert_eq!(response.result().unwrap()["removed"], true);
    assert!(state::load_registry(&home.registry_json())
        .unwrap()
        .entries
        .is_empty());
    std::fs::remove_dir_all(home.root()).ok();
}
