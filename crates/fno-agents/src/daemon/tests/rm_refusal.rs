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
