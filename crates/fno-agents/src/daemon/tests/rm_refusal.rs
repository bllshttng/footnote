//! The rm live-row refusal per roster verdict: unknown read, partial list.

use super::*;

/// The Unknown-roster refusal must name the read's own reason. A bare
/// "the roster read failed" sent the 2026-09-08 operator to retry a
/// 15s timeout, which reproduces forever.
#[tokio::test]
async fn rm_unknown_roster_refusal_names_the_reads_own_reason() {
    let home = short_home("rmunknown");
    let mut row = claude_rm_row(
        "done-worker",
        "aaabbb13",
        "aaabbb13-1111-2222-3333-444444444444",
    );
    row.status = AgentStatus::Live;
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "done-worker"}));
    let response = handle_rm_with(
        &ctx,
        &request,
        &|| crate::claude_roster::ClaudeAgentsSnapshot::Unknown {
            rows: Vec::new(),
            warnings: vec!["claude agents --json --all timed out after 15s".into()],
        },
        &|_| Ok(()),
        &|_| true,
        &|_, _| Ok(true),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    let message = &response.error().unwrap().message;
    assert!(message.contains("the roster read failed:"));
    assert!(message.contains("timed out after 15s"));
    std::fs::remove_dir_all(home.root()).ok();
}

/// A partial list cannot prove absence: a row hidden among the skipped
/// rows would read as gone. The refusal names the warnings instead.
#[tokio::test]
async fn rm_absence_is_not_proof_on_a_warning_carrying_list() {
    let home = short_home("rmpartialabsence");
    let mut row = claude_rm_row(
        "done-worker",
        "aaabbb14",
        "aaabbb14-1111-2222-3333-444444444444",
    );
    row.status = AgentStatus::Live;
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "done-worker"}));
    let response = handle_rm_with(
        &ctx,
        &request,
        &|| crate::claude_roster::ClaudeAgentsSnapshot::Known {
            rows: Vec::new(),
            warnings: vec!["one malformed row".into()],
        },
        &|_| Ok(()),
        &|_| true,
        &|_, _| Ok(true),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    let message = &response.error().unwrap().message;
    assert!(message.contains("absence is not proof"));
    assert_eq!(
        state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .len(),
        1
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// The reaper's dead-pid verdict must clear this gate too: a merge cleanup
/// that accepted the evidence is refused one verb later if rm does not read
/// it, and the refused cleanup tombstones its request.
#[tokio::test]
async fn rm_accepts_a_dead_pid_as_provably_gone() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = short_home("rmdeadpid");
    let mut row = claude_rm_row(
        "finished-worker",
        "aaabbb15",
        "aaabbb15-1111-2222-3333-444444444444",
    );
    row.status = AgentStatus::Live;
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "finished-worker"}));
    let first = std::sync::atomic::AtomicBool::new(true);
    // i32::MAX cannot name a live process on any supported platform (and it
    // casts to a positive pid_t, so kill reads it as an existence probe), so the
    // ESRCH probe must answer "gone".
    let response = handle_rm_with(
        &ctx,
        &request,
        &|| {
            if first.swap(false, std::sync::atomic::Ordering::Relaxed) {
                crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                    crate::claude_roster::ClaudeAgentRow::new("aaabbb15", Some("working"))
                        .with_pid(Some(i32::MAX as u32)),
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

    assert!(
        response.error().is_none(),
        "dead-pid evidence must clear the rm live gate: {:?}",
        response.error().map(|e| e.message.clone())
    );
    assert_eq!(
        state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .len(),
        0
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// Roster `blocked` is claude's "Needs input", not a model outage. rm runs
/// its own `claude stop` first (AC4-ERR); when the roster still lists the
/// row, the refusal says the stop ran and never advises a separate verb.
#[tokio::test]
async fn rm_refuses_a_blocked_claude_row_after_its_own_stop() {
    let home = short_home("rmblocked");
    let mut row = claude_rm_row(
        "blocked-worker",
        "dddd4444",
        "dddd4444-1111-2222-3333-444444444444",
    );
    row.status = AgentStatus::Live;
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "blocked-worker"}));
    let stops = std::sync::atomic::AtomicUsize::new(0);
    let response = handle_rm_with(
        &ctx,
        &request,
        &|| {
            crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                crate::claude_roster::ClaudeAgentRow::new("dddd4444", Some("blocked")),
            ])
        },
        &|_| panic!("the refusal returns before the harness cascade"),
        &|_| {
            stops.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            true
        },
        &|_, _| panic!("the refusal returns before the pane arms"),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert_eq!(stops.load(std::sync::atomic::Ordering::Relaxed), 1);
    let message = &response.error().unwrap().message;
    assert!(message.contains("rm ran `claude stop`"), "{message}");
    assert!(
        !message.contains("fno agents stop"),
        "must not advise a separate stop verb: {message}"
    );
    assert!(!message.contains("rotate"), "{message}");
    assert!(!message.contains("model outage"), "{message}");
    assert_eq!(
        state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .len(),
        1
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// `--force` reaches a blocked row: a session that never got a prompt waits
/// for input forever, and force is the documented way out.
#[tokio::test]
async fn rm_force_removes_a_blocked_claude_row() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = short_home("rmblockedforce");
    let mut row = claude_rm_row(
        "blocked-worker",
        "dddd4445",
        "dddd4445-1111-2222-3333-444444444444",
    );
    row.status = AgentStatus::Live;
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(
        1,
        "agent.rm",
        json!({"name": "blocked-worker", "force": true}),
    );
    let first = std::sync::atomic::AtomicBool::new(true);
    let removed = std::sync::atomic::AtomicBool::new(false);
    let response = handle_rm_with(
        &ctx,
        &request,
        &|| {
            if first.swap(false, std::sync::atomic::Ordering::Relaxed) {
                crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                    crate::claude_roster::ClaudeAgentRow::new("dddd4445", Some("blocked")),
                ])
            } else {
                crate::claude_roster::ClaudeAgentsSnapshot::known(Vec::new())
            }
        },
        &|id| {
            assert_eq!(id, "dddd4445");
            removed.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        },
        &|_| true,
        &|_, _| Ok(true),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert!(
        response.error().is_none(),
        "force must reach a blocked row: {:?}",
        response.error().map(|e| e.message.clone())
    );
    assert!(removed.load(std::sync::atomic::Ordering::Relaxed));
    assert_eq!(
        state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .len(),
        0
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// A live row with a mux ref is ended by rm's own mux pane kill. When the
/// kill fails, the row stays and the refusal quotes the kill's reason -
/// never a stop verb (AC3-ERR's mux-arm shape).
#[tokio::test]
async fn rm_keeps_the_row_when_the_mux_pane_kill_fails() {
    let home = short_home("rmpanekill");
    let mut row = ask_row("pane-worker", Some("2020-01-01T00:00:00Z"));
    row.status = AgentStatus::Live;
    row.harness = Some("codex".into());
    row.mux = Some(state::MuxRef {
        session: "main".into(),
        pane_id: 33,
    });
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "pane-worker"}));
    let response = handle_rm_with(
        &ctx,
        &request,
        &|| panic!("a non-claude row must not read the claude roster"),
        &|_| panic!("rm must not reach claude rm"),
        &|_| panic!("no claude stop may run for a non-claude row"),
        &|session, pane_id| {
            assert_eq!((session, pane_id), ("main", 33));
            Err("pane server unreachable".into())
        },
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    let message = &response.error().unwrap().message;
    assert!(
        message.contains("mux pane main:33 removal failed: pane server unreachable"),
        "must quote the kill reason: {message}"
    );
    assert!(
        !message.contains("fno agents stop"),
        "must not advise stop: {message}"
    );
    assert_eq!(
        state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .len(),
        1
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// AC3-ERR: a live pane-substrate row whose pid cannot be proven keeps its
/// registry row; the refusal quotes the pane stop's own reason and never
/// advises `fno agents stop`.
#[tokio::test]
async fn rm_keeps_the_row_when_the_pane_stop_cannot_be_proven() {
    let home = short_home("rmnomux");
    let mut row = ask_row("pane-row", Some("2020-01-01T00:00:00Z"));
    row.status = AgentStatus::Live;
    row.substrate = Some("pane".into());
    row.harness = Some("claude".into());
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "pane-row"}));
    let response = handle_rm_with(
        &ctx,
        &request,
        &|| crate::claude_roster::ClaudeAgentsSnapshot::known(Vec::new()),
        &|_| panic!("rm must not reach claude rm"),
        &|_| panic!("no claude stop may run for a pane-substrate row"),
        &|_, _| panic!("a pane-substrate row takes the pane arm, not the mux kill"),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    let message = &response.error().unwrap().message;
    assert!(
        message.contains("the pane stop did not confirm"),
        "{message}"
    );
    assert!(
        message.contains("no verified pid"),
        "quotes the pane stop's own reason: {message}"
    );
    assert!(
        !message.contains("fno agents stop"),
        "must not advise stop: {message}"
    );
    assert_eq!(
        state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .len(),
        1
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// A mux ref whose session is empty cannot name a runnable kill command,
/// and the old refusal printed one with a hole in it (`fno mux pane kill
/// :33`). The kill still runs through the daemon's own seam; on failure the
/// row stays and no hole-y command is printed.
#[tokio::test]
async fn rm_keeps_the_row_when_the_mux_ref_has_no_session() {
    let home = short_home("rmmalformedmux");
    let mut row = ask_row("orphan-pane-worker", Some("2020-01-01T00:00:00Z"));
    row.status = AgentStatus::Live;
    row.harness = Some("codex".into());
    row.mux = Some(state::MuxRef {
        session: String::new(),
        pane_id: 33,
    });
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "orphan-pane-worker"}));
    let response = handle_rm_with(
        &ctx,
        &request,
        &|| panic!("a non-claude row must not read the claude roster"),
        &|_| panic!("rm must not reach claude rm"),
        &|_| panic!("no claude stop may run for a non-claude row"),
        &|_, _| Err("pane server unreachable".into()),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    let message = &response.error().unwrap().message;
    assert!(
        message.contains("removal failed: pane server unreachable"),
        "{message}"
    );
    assert!(
        !message.contains("fno mux pane kill"),
        "must not print a command with a hole in it: {message}"
    );
    assert_eq!(
        state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .len(),
        1
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// AC2 (no-actor half): a live codex thread row is removed by rm itself -
/// the interrupt-settle leg runs in the teardown (a row with no hosted
/// actor settles as `no-turn`), and no stop verb, roster read, or pane
/// kill is involved. CODEX_HOME rides the fake daemon so the codex index
/// capture never touches the real one.
#[tokio::test(flavor = "current_thread")]
async fn rm_ends_a_live_codex_thread_row_by_itself() {
    let _guard = crate::path_test_guard();
    let _daemon = crate::codex_fake_daemon::FakeDaemon::start(
        crate::codex_fake_daemon::Behavior::quick().with_thread_id("thread-rm-alone"),
    );
    let home = short_home("rmthreadalone");
    let row = thread_entry("t-rm-alone", AgentStatus::Live, None);
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent"));
    let request = Request::new(1, "agent.rm", json!({"name": "t-rm-alone"}));
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
async fn rm_removes_a_live_pane_row_when_the_probe_is_unknown() {
    // The probe no longer gates: a mux-ref live row's process end is rm's
    // own mux pane kill, and its answer decides. A kill that removed the
    // pane removes the row with no --force.
    let home = short_home("rmpaneunknown");
    let mut row = ask_row("maybe-pane-worker", Some("2020-01-01T00:00:00Z"));
    row.harness = Some("opencode".into());
    row.status = AgentStatus::Live;
    row.mux = Some(state::MuxRef {
        session: "main".into(),
        pane_id: 76,
    });
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "maybe-pane-worker"}));

    let response = handle_rm_with(
        &ctx,
        &request,
        &|| panic!("non-Claude row must not read the Claude list"),
        &|_| panic!("non-Claude row must not call claude rm"),
        &|_| panic!("no claude stop may run for a non-claude row"),
        &|session, pane_id| {
            assert_eq!((session, pane_id), ("main", 76));
            Ok(true)
        },
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

#[tokio::test]
async fn rm_removes_a_live_pane_row_whose_probe_says_present() {
    // A probe that still sees the pane does not gate: rm kills the pane
    // itself, and the kill's already-absent answer still confirms (the pane
    // died between the probe and the kill).
    let home = short_home("rmpanepresent");
    let mut row = ask_row("live-pane-worker", Some("2020-01-01T00:00:00Z"));
    row.harness = Some("opencode".into());
    row.status = AgentStatus::Live;
    row.mux = Some(state::MuxRef {
        session: "main".into(),
        pane_id: 76,
    });
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "live-pane-worker"}));

    let response = handle_rm_with(
        &ctx,
        &request,
        &|| panic!("non-Claude row must not read the Claude list"),
        &|_| panic!("non-Claude row must not call claude rm"),
        &|_| panic!("no claude stop may run for a non-claude row"),
        &|session, pane_id| {
            assert_eq!((session, pane_id), ("main", 76));
            Ok(false)
        },
        &|_, _| PaneProbe::Present,
    )
    .await;

    assert!(
        response.error().is_none(),
        "{:?}",
        response.error().map(|e| e.message.clone())
    );
    assert_eq!(response.result().unwrap()["removed"], true);
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
async fn rm_still_refuses_an_idle_claude_row() {
    let home = short_home("rmidle");
    let mut row = claude_rm_row(
        "idle-worker",
        "1d1e0001",
        "1d1e0001-1111-2222-3333-444444444444",
    );
    row.status = AgentStatus::Live;
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "idle-worker"}));

    let response = handle_rm_with(
        &ctx,
        &request,
        &|| {
            crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                crate::claude_roster::ClaudeAgentRow::new("1d1e0001", Some("idle")),
            ])
        },
        &|_| panic!("an idle row must not reach claude rm"),
        &|_| true,
        &|_, _| panic!("an idle row must not reach mux kill"),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert!(response.error().unwrap().message.contains("still live"));
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
async fn rm_refuses_a_stored_live_row_when_the_roster_is_unknown() {
    // AC2-EDGE: an Unknown snapshot (the shellout failed, timed out, or
    // parsed badly) is not proof of anything; keep refusing.
    let home = short_home("rmrosterunknown");
    let mut row = claude_rm_row(
        "maybe-live",
        "aaaa9999",
        "aaaa9999-1111-2222-3333-444444444444",
    );
    row.status = AgentStatus::Live;
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "maybe-live"}));
    let stops = std::sync::atomic::AtomicUsize::new(0);

    let response = handle_rm_with(
        &ctx,
        &request,
        &|| crate::claude_roster::ClaudeAgentsSnapshot::unknown("list timed out"),
        &|_| panic!("an unknown roster must not reach claude rm"),
        &|_| {
            stops.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            true
        },
        &|_, _| panic!("an unknown roster must not reach mux kill"),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert!(response.error().is_some());
    assert_eq!(stops.load(std::sync::atomic::Ordering::Relaxed), 1);
    {
        // Positive markers (x-d19e): the unprovable case names the retry,
        // states what forcing costs, and offers no override flag.
        let message = &response.error().unwrap().message;
        assert!(
            message.contains("retry once that read succeeds"),
            "{}",
            message
        );
        assert!(
            message.contains("rm already ran `claude stop`"),
            "{}",
            message
        );
        assert!(message.contains("resume handle"), "{}", message);
        assert!(!message.contains("--force"), "{}", message);
    }
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
async fn rm_refuses_a_row_the_roster_still_carries_and_names_no_force() {
    // AC2-NEG + AC2-COV: the short id IS present in a known snapshot, so
    // the row really is live. The refusal names the working incantation
    // (claude takes the short id, not the agent name) and never --force,
    // which a king previously read as the remedy and applied to five
    // genuinely-live rows.
    let home = short_home("rmstilllive");
    let mut row = claude_rm_row(
        "genuinely-live",
        "bbbb8888",
        "bbbb8888-1111-2222-3333-444444444444",
    );
    row.status = AgentStatus::Live;
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "genuinely-live"}));
    let stops = std::sync::atomic::AtomicUsize::new(0);

    let response = handle_rm_with(
        &ctx,
        &request,
        &|| {
            crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                crate::claude_roster::ClaudeAgentRow::new("bbbb8888", Some("running")),
            ])
        },
        &|_| panic!("a genuinely live row must not reach claude rm"),
        &|_| {
            stops.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            true
        },
        &|_, _| panic!("a genuinely live row must not reach mux kill"),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert_eq!(stops.load(std::sync::atomic::Ordering::Relaxed), 1);
    let message = &response.error().unwrap().message;
    assert!(message.contains("claude agents --json --all"));
    assert!(message.contains("rm ran `claude stop`"), "{}", message);
    // Specimen guard (x-d19e): the refusal names what rm itself ran, the
    // hand-teardown cost, and offers no override flag. A rewrite that
    // drops any of the three must fail here, not in a king's reign.
    assert!(
        message.contains("rm makes the same call itself"),
        "{}",
        message
    );
    assert!(message.contains("spends the resume handle"), "{}", message);
    // This refusal used to offer `claude stop <row>` then `claude rm <row>`
    // as a by-hand alternative, and this test required it. Ruling
    // d-1900e419 retired that pair: the harness row IS the resume handle,
    // and dropping it by hand spends the handle for nothing rm has not
    // already done. The refusal must not teach it back.
    // retired-ok: asserts the retired pair is ABSENT from the refusal.
    assert!(!message.contains("claude stop bbbb8888"));
    // retired-ok: asserts the retired pair is ABSENT from the refusal.
    assert!(!message.contains("claude rm bbbb8888"));
    assert!(!message.contains("fno agents stop"), "{}", message);
    assert!(!message.contains("--force"));
    assert!(!message.contains("-F"));
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
async fn rm_refusal_on_a_claude_row_without_a_row_id_names_no_stop_and_no_force() {
    // x-d19e + law d-81c6da7e: with no resolvable row id, rm cannot run
    // its claude stop either, so the refusal names that and the cost of
    // forcing through - never the override flag, never a stop verb.
    let home = short_home("rmnorowid");
    // short_id empty AND session id empty (a pid carries the handle
    // invariant instead): claude_row_id answers None, which is the arm
    // where the roster can never be consulted.
    let mut row = ask_row("idless-live", Some("2020-01-01T00:00:00Z"));
    row.harness = Some("claude".into());
    row.harness_session_id = None;
    row.pid = Some(4242);
    row.pid_start_time = Some(123456);
    row.status = AgentStatus::Live;
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "idless-live"}));
    let stops = std::sync::atomic::AtomicUsize::new(0);

    let response = handle_rm_with(
        &ctx,
        &request,
        &|| crate::claude_roster::ClaudeAgentsSnapshot::known(Vec::new()),
        &|_| panic!("an unresolvable row must not reach claude rm"),
        &|_| {
            stops.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            true
        },
        &|_, _| panic!("an unresolvable row must not reach mux kill"),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    assert_eq!(stops.load(std::sync::atomic::Ordering::Relaxed), 0);
    let message = &response.error().expect("still live must refuse").message;
    assert!(
        message.contains("no resolvable harness row id"),
        "{}",
        message
    );
    assert!(!message.contains("fno agents stop"), "{}", message);
    assert!(message.contains("resume handle"), "{}", message);
    assert!(!message.contains("--force"), "{}", message);
    assert_eq!(
        state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .len(),
        1
    );
    std::fs::remove_dir_all(home.root()).ok();
}
