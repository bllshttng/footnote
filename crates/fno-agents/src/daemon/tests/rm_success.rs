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
    let manifest = project.join(".fno/kings/alpha.md");
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
        &|_, _| Ok(true),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert!(response.error().is_none(), "{response:?}");
    assert!(
        !manifest.exists(),
        "successful rm left crown loop state behind"
    );
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
    let manifest = project.join(".fno/kings/alpha.md");
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
        &|_, _| Ok(true),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert!(response.error().is_none(), "{response:?}");
    assert!(
        manifest.exists(),
        "rm deleted a manifest naming a different session: the live successor's gate"
    );
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
        &|_| Err("x".repeat(crate::events::MAX_EVENT_PAYLOAD_BYTES * 2)),
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
