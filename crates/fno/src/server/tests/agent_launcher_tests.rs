//! The server-side launch coordinator: pre-birth validation through
//! the Core, desk dedup/replay, and the trusted-update path. Subprocess
//! journeys live in `tests/agent_launcher_journey.rs`.
use super::*;

fn launch_req(id: u64, cwd: &str, harness: &str) -> crate::proto::AgentLaunchRequest {
    crate::proto::AgentLaunchRequest {
        request_id: id,
        revision: 1,
        cwd: cwd.to_string(),
        harness: harness.to_string(),
        substrate: "pane".to_string(),
        model: None,
        model_names_harness: false,
        effort: None,
        permission_mode: None,
        placement: None,
        portal: None,
        split: None,
        node: None,
        message: String::new(),
    }
}

#[tokio::test]
async fn agent_launch_accepts_empty_substrate_and_refuses_headless() {
    // AC6: EMPTY = the door's default, an explicit lane still names pane or
    // thread, and headless is refused pre-wire (never offered by the dock).
    let (out_tx, _out_rx) = mpsc::channel::<(u64, PaneChunk)>(8);
    let (exit_tx, _exit_rx) = mpsc::channel::<u64>(8);
    let (self_tx, _self_rx) = mpsc::channel::<CoreMsg>(8);
    let mut core = empty_core_with(self_tx);
    // The empty substrate passes substrate validation; the BAD PATH refusal
    // (below) must then be the cwd one, not the substrate one.
    let mut req = launch_req(1, "/definitely/not/a/dir/", "claude");
    req.substrate = String::new();
    core.agent_launch(1, req);
    match core.launch_desk.settled_state(1, 1) {
        Some(crate::proto::agent_launch::LaunchState::Refused { reason }) => {
            assert!(
                reason.contains("does not exist"),
                "empty substrate accepted; refusal should be the cwd: {reason}"
            );
        }
        other => panic!("expected a settled refusal, got {other:?}"),
    }
    let mut bad = launch_req(2, "/definitely/not/a/dir/", "claude");
    bad.substrate = "headless".to_string();
    core.agent_launch(1, bad);
    match core.launch_desk.settled_state(1, 2) {
        Some(crate::proto::agent_launch::LaunchState::Refused { reason }) => {
            assert!(
                reason.contains("substrate"),
                "headless refuses pre-wire: {reason}"
            );
        }
        other => panic!("expected a substrate refusal, got {other:?}"),
    }
    drop(out_tx);
    drop(exit_tx);
}

#[tokio::test]
async fn agent_launch_refuses_pre_birth_and_settles_the_desk() {
    let (out_tx, _out_rx) = mpsc::channel::<(u64, PaneChunk)>(8);
    let (exit_tx, _exit_rx) = mpsc::channel::<u64>(8);
    let (self_tx, mut self_rx) = mpsc::channel::<CoreMsg>(8);
    let mut core = empty_core_with(self_tx);
    // A nonexistent project path: refused BEFORE any effect - no spawn task
    // ever runs, so no AgentLaunchUpdate can arrive on the core channel.
    core.agent_launch(1, launch_req(1, "/definitely/not/a/dir/", "claude"));
    let state = core.launch_desk.settled_state(1, 1);
    match state {
        Some(crate::proto::agent_launch::LaunchState::Refused { reason }) => {
            assert!(reason.contains("does not exist"), "reason: {reason}");
        }
        other => panic!("expected a settled refusal, got {other:?}"),
    }
    drop(out_tx);
    drop(exit_tx);
    // The core channel stays empty: nothing was spawned.
    assert!(
        self_rx.try_recv().is_err(),
        "a pre-birth refusal never spawns a task"
    );
}

#[tokio::test]
async fn duplicate_request_id_replays_without_a_second_attempt() {
    let (out_tx, _out_rx) = mpsc::channel::<(u64, PaneChunk)>(8);
    let (exit_tx, _exit_rx) = mpsc::channel::<u64>(8);
    let (self_tx, _self_rx) = mpsc::channel::<CoreMsg>(8);
    let mut core = empty_core_with(self_tx);
    let bad = "/definitely/not/a/dir/";
    core.agent_launch(1, launch_req(1, bad, "claude"));
    core.agent_launch(1, launch_req(1, bad, "claude"));
    // The second submission hit the finished map and replayed; the desk
    // still holds exactly one terminal state for the id.
    assert!(matches!(
        core.launch_desk.settled_state(1, 1),
        Some(crate::proto::agent_launch::LaunchState::Refused { .. })
    ));
    drop(out_tx);
    drop(exit_tx);
}

#[tokio::test]
async fn agent_launch_refuses_an_invalid_node_id_pre_birth() {
    // AC3-ERR: a board prefill's node id becomes an argv element, so an
    // empty id, a flag-shaped id and a splatting id all refuse BEFORE any
    // effect - the desk settles the refusal and no task ever spawns.
    let (out_tx, _out_rx) = mpsc::channel::<(u64, PaneChunk)>(8);
    let (exit_tx, _exit_rx) = mpsc::channel::<u64>(8);
    let (self_tx, mut self_rx) = mpsc::channel::<CoreMsg>(8);
    let mut core = empty_core_with(self_tx);
    for (i, bad) in ["", "-rf", "x 1"].iter().enumerate() {
        let mut req = launch_req(i as u64 + 1, "/tmp", "claude");
        req.node = Some(bad.to_string());
        core.agent_launch(i as u64 + 1, req);
        match core.launch_desk.settled_state(i as u64 + 1, i as u64 + 1) {
            Some(crate::proto::agent_launch::LaunchState::Refused { reason }) => {
                assert!(
                    reason.contains("invalid node id"),
                    "refusal names the bad id {bad:?}: {reason}"
                );
            }
            other => panic!("expected a settled refusal for {bad:?}, got {other:?}"),
        }
    }
    drop(out_tx);
    drop(exit_tx);
    assert!(
        self_rx.try_recv().is_err(),
        "a pre-birth refusal never spawns a task"
    );
}

fn empty_core_with(self_tx: mpsc::Sender<CoreMsg>) -> Core {
    // Reuse the shared fixture body, then override the self channel so the
    // launch task can route updates without a live server loop.
    let mut core = empty_core();
    core.self_tx = self_tx;
    core
}
