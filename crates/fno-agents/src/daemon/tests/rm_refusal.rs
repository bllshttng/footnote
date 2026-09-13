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

/// Roster `blocked` is claude's "Needs input", not a model outage. A plain rm
/// refuses through the live-row gate and names a verb the CLI has.
#[tokio::test]
async fn rm_refuses_a_blocked_claude_row_through_the_live_gate() {
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
    let response = handle_rm_with(
        &ctx,
        &request,
        &|| {
            crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                crate::claude_roster::ClaudeAgentRow::new("dddd4444", Some("blocked")),
            ])
        },
        &|_| panic!("a plain rm must not reach claude rm"),
        &|_, _| panic!("a plain rm must not reach mux kill"),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    let message = &response.error().unwrap().message;
    assert!(
        message.contains("fno agents stop blocked-worker"),
        "{message}"
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

/// A live non-claude row carrying a usable mux ref is a pane worker, and
/// stop cannot serve one (ruling d-658e6834): the refusal must name the
/// pane-kill path with the row's real session and pane id, and say the
/// registry row survives the kill.
#[tokio::test]
async fn rm_refusal_names_the_pane_kill_for_a_live_pane_worker() {
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
        &|_| panic!("a refusal must not reach claude rm"),
        &|_, _| panic!("a refusal must not reach mux kill"),
        &|session, pane_id| {
            assert_eq!((session, pane_id), ("main", 33));
            PaneProbe::Unknown
        },
    )
    .await;

    let message = &response.error().unwrap().message;
    assert!(
        message.contains("fno mux pane kill main:33"),
        "must name the pane-kill command: {message}"
    );
    assert!(
        message.contains("registry row survives"),
        "must say the row survives the kill: {message}"
    );
    assert!(
        !message.contains("fno agents stop"),
        "must not advise stop for a pane worker: {message}"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// A live non-claude row with no mux ref keeps today's message byte for
/// byte: stop is the right verb for a row with no pane.
#[tokio::test]
async fn rm_refusal_keeps_todays_stop_message_for_a_row_without_a_mux_ref() {
    let home = short_home("rmnomux");
    let mut row = ask_row("thread-worker", Some("2020-01-01T00:00:00Z"));
    row.status = AgentStatus::Live;
    row.harness = Some("codex".into());
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "thread-worker"}));
    let response = handle_rm_with(
        &ctx,
        &request,
        &|| panic!("a non-claude row must not read the claude roster"),
        &|_| panic!("a refusal must not reach claude rm"),
        &|_, _| panic!("a refusal must not reach mux kill"),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    let message = &response.error().unwrap().message;
    assert_eq!(
        message.as_str(),
        "agent thread-worker is still live. Stop it with `fno agents stop thread-worker`; \
         rm proceeds on its own once the row is gone. Forcing it through orphans a live \
         process and spends the row's resume handle. If stop answers no_op (no addressable \
         session behind the row), the row cannot prove liveness either way; the override \
         for that case is documented in `fno agents rm --help`, not here."
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// A mux ref whose session is empty cannot print a runnable command, and a
/// refusal with a hole in it (`fno mux pane kill :33`) is worse than one
/// naming the wrong verb: fall back to today's message.
#[tokio::test]
async fn rm_refusal_falls_back_to_stop_when_the_mux_ref_has_no_session() {
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
        &|_| panic!("a refusal must not reach claude rm"),
        &|_, _| panic!("a refusal must not reach mux kill"),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    let message = &response.error().unwrap().message;
    assert!(
        message.contains("fno agents stop orphan-pane-worker"),
        "must fall back to today's message: {message}"
    );
    assert!(
        !message.contains("fno mux pane kill"),
        "must not print a command with a hole in it: {message}"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// A whitespace-only session is the same malformed-ref class: no runnable
/// address, so the refusal keeps today's message.
#[tokio::test]
async fn rm_refusal_falls_back_when_the_mux_session_is_blank() {
    let home = short_home("rmblankmux");
    let mut row = ask_row("blank-pane-worker", Some("2020-01-01T00:00:00Z"));
    row.status = AgentStatus::Live;
    row.harness = Some("codex".into());
    row.mux = Some(state::MuxRef {
        session: "   ".into(),
        pane_id: 33,
    });
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.rm", json!({"name": "blank-pane-worker"}));
    let response = handle_rm_with(
        &ctx,
        &request,
        &|| panic!("a non-claude row must not read the claude roster"),
        &|_| panic!("a refusal must not reach claude rm"),
        &|_, _| panic!("a refusal must not reach mux kill"),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    let message = &response.error().unwrap().message;
    assert!(
        message.contains("fno agents stop blank-pane-worker"),
        "must fall back to today's message: {message}"
    );
    assert!(
        !message.contains("fno mux pane kill"),
        "must not print a command with a hole in it: {message}"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn rm_still_refuses_a_stored_live_pane_row_when_the_probe_is_unknown() {
    // Fail-closed: a probe that errored, timed out, or parsed badly proves
    // nothing. The refusal and the row both stay.
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
        &|_, _| panic!("a refused row must not reach the pane kill"),
        &|_, _| PaneProbe::Unknown,
    )
    .await;

    let error = response.error().expect("a stored-live row must be refused");
    assert!(error.message.contains("still live"), "{}", error.message);
    // Positive markers (x-d19e): the safe verb for a mux-ref row is the
    // pane kill (d-658e6834); the override lives in --help, never here.
    assert!(
        error.message.contains("fno mux pane kill main:76"),
        "{}",
        error.message
    );
    assert!(
        error.message.contains("stop cannot serve it"),
        "{}",
        error.message
    );
    assert!(!error.message.contains("--force"), "{}", error.message);
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
async fn rm_still_refuses_a_stored_live_pane_row_when_the_pane_is_present() {
    // A live pane is a live worker; the refusal must stand.
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
        &|_, _| panic!("a refused row must not reach the pane kill"),
        &|_, _| PaneProbe::Present,
    )
    .await;

    let error = response.error().expect("a stored-live row must be refused");
    assert!(error.message.contains("still live"), "{}", error.message);
    // Positive markers (x-d19e): same contract as the probe-unknown arm.
    assert!(
        error.message.contains("fno mux pane kill main:76"),
        "{}",
        error.message
    );
    assert!(
        error.message.contains("stop cannot serve it"),
        "{}",
        error.message
    );
    assert!(!error.message.contains("--force"), "{}", error.message);
    assert_eq!(
        state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .len(),
        1
    );
    std::fs::remove_dir_all(home.root()).ok();
}
