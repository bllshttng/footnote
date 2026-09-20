//! The keeper-rebind strategy: the child never stops.
//!
//! A keeper-lane pane's PTY already lives in a `fno-agents-worker --pane`
//! process, which is the same binary a thread keeper runs on. The two lanes
//! differ in exactly two ways: which directory the socket sits in, and
//! whether the mux server holds the subscriber seat. So the conversion is a
//! rename plus a seat release plus a row flip, with no relaunch anywhere.
//!
//! The premise is measured rather than assumed: a renamed unix socket path
//! still reaches the same listener (macOS 25.3 - the new path answers, the
//! old path refuses with ENOENT), which is what lets the daemon's keeper
//! sweep rebind the row by socket path afterward.

use crate::convert::{ConvertHost, ConvertPlan};

/// What the rebind did, for the receipt. Every field is a fact read back
/// AFTER the move, never one assumed from the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebindOutcome {
    pub socket: String,
    pub keeper_pid: u32,
    pub child_pid: u32,
    pub session_id: String,
}

/// The thread-lane socket this row's keeper moves to. Named for the ROW, not
/// the pane: the pane id dies with the hand-off, and the row name is what
/// every later reader (the sweep, `fno mux thread <name>`) addresses.
pub fn thread_socket_path(state_root: &std::path::Path, name: &str) -> std::path::PathBuf {
    state_root
        .join("mux")
        .join("threads")
        .join(format!("{name}.sock"))
}

/// Ask the mux server to hand the pane off. Returns the target path on
/// success. Separated from the probe and the flip so a test drives each
/// step, and so the caller can tell a refusal (nothing moved) from a
/// half-move (the socket moved, the row did not).
pub fn hand_off(
    plan: &ConvertPlan,
    target: &std::path::Path,
    run: &dyn Fn(&str, u64, &str) -> Result<(), String>,
) -> Result<(), String> {
    let ConvertHost::KeeperPane { .. } = plan.host else {
        return Err(format!(
            "{} is not keeper-hosted; the rebind strategy has no socket to move",
            plan.name
        ));
    };
    let (session, pane_id) = plan.host.pane();
    run(session, pane_id, &target.to_string_lossy())
}

/// The production hand-off: `fno mux pane kill --hand-off-to`. It rides the
/// kill verb because from the server's side it IS the same question - this
/// pane stops being ours - and a second verb would duplicate the pane
/// resolution and refusal ladder.
pub fn run_mux_hand_off(session: &str, pane_id: u64, target: &str) -> Result<(), String> {
    let pane_id = pane_id.to_string();
    let run = crate::pane_stop::run_fno(&[
        "mux",
        "pane",
        "kill",
        "--server",
        session,
        &pane_id,
        "--hand-off-to",
        target,
    ])?;
    if run.ok {
        return Ok(());
    }
    Err(format!(
        "mux pane kill --hand-off-to refused: {}",
        run.stderr
    ))
}

/// Read the moved socket back and assert it is the SAME keeper holding the
/// SAME child on the SAME session. A hand-off that answered success but
/// moved something else is exactly what this re-read exists to catch: the
/// row is about to be pointed at whatever is behind this path.
pub fn verify_moved(
    plan: &ConvertPlan,
    identify: &serde_json::Value,
) -> Result<RebindOutcome, String> {
    let expected_child = plan.host.child_pid();
    let child_pid = identify
        .get("child_pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .ok_or_else(|| "the moved keeper answered no child pid".to_string())?;
    if child_pid != expected_child {
        return Err(format!(
            "the moved keeper holds child {child_pid}, but {} was converted at child \
             {expected_child}; refusing to bind the row to another process",
            plan.name
        ));
    }
    let argv: Vec<String> = identify
        .get("argv")
        .and_then(serde_json::Value::as_array)
        .map(|argv| {
            argv.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let session_id = crate::pane_keeper::session_id_from_argv(&argv)
        .ok_or_else(|| "the moved keeper answers no session id".to_string())?;
    if session_id != plan.session_id {
        return Err(format!(
            "the moved keeper holds session {session_id}, but {} was converted at \
             {}; refusing to bind the row to another session",
            plan.name, plan.session_id
        ));
    }
    let keeper_pid = identify
        .get("keeper_pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .ok_or_else(|| "the moved keeper answered no keeper pid".to_string())?;
    Ok(RebindOutcome {
        socket: String::new(),
        keeper_pid,
        child_pid,
        session_id,
    })
}

/// The row as the thread lane reads it. Applied through the registry write
/// path by the caller; kept as a pure function so the shape is testable
/// without a registry.
pub fn flipped_row(entry: &mut crate::state::RegistryEntry, outcome: &RebindOutcome) {
    entry.substrate = Some("thread".to_string());
    entry.mux = None;
    entry.messaging_socket_path = Some(outcome.socket.clone());
    // `pid` is the KEEPER for a thread row, and `keeper_child_pid` the
    // process the operator sees. A row that left them swapped would have
    // the liveness ladder probing the wrong process.
    entry.pid = Some(outcome.keeper_pid);
    entry.keeper_child_pid = Some(outcome.child_pid);
    entry.host_mode = Some(crate::state::HOST_MODE_INTERACTIVE.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::ConvertHost;

    fn plan() -> ConvertPlan {
        ConvertPlan {
            name: "king-delivery".to_string(),
            harness: "pi".to_string(),
            strategy: "keeper-rebind".to_string(),
            preserves_id: true,
            session_id: "11111111-2222-3333-4444-555555555555".to_string(),
            host: ConvertHost::KeeperPane {
                session: "fno".to_string(),
                pane_id: 2313,
                keeper_socket: "/state/mux/panes/fno-2313.sock".to_string(),
                keeper_pid: 900,
                child_pid: 4242,
            },
            steps: vec![],
        }
    }

    fn identify(child_pid: u32, session_id: &str, keeper_pid: u32) -> serde_json::Value {
        serde_json::json!({
            "keeper_pid": keeper_pid,
            "child_pid": child_pid,
            "argv": ["pi", "--session-id", session_id],
        })
    }

    #[test]
    fn the_thread_socket_is_named_for_the_row_not_the_pane() {
        // The pane id dies with the hand-off; the row name is what the
        // sweep and `fno mux thread <name>` address afterward.
        let path = thread_socket_path(std::path::Path::new("/state"), "king-delivery");
        assert_eq!(
            path,
            std::path::PathBuf::from("/state/mux/threads/king-delivery.sock")
        );
    }

    #[test]
    fn the_hand_off_addresses_the_live_pane_and_the_target() {
        let seen = std::cell::RefCell::new(Vec::new());
        let run = |session: &str, pane: u64, target: &str| {
            seen.borrow_mut()
                .push((session.to_string(), pane, target.to_string()));
            Ok(())
        };
        hand_off(
            &plan(),
            std::path::Path::new("/state/mux/threads/king-delivery.sock"),
            &run,
        )
        .expect("the hand-off runs");
        assert_eq!(
            seen.into_inner(),
            vec![(
                "fno".to_string(),
                2313,
                "/state/mux/threads/king-delivery.sock".to_string()
            )]
        );
    }

    #[test]
    fn a_moved_keeper_holding_the_same_child_and_session_verifies() {
        let outcome = verify_moved(
            &plan(),
            &identify(4242, "11111111-2222-3333-4444-555555555555", 900),
        )
        .expect("the same keeper verifies");
        assert_eq!(outcome.child_pid, 4242);
        assert_eq!(outcome.keeper_pid, 900);
        assert_eq!(outcome.session_id, "11111111-2222-3333-4444-555555555555");
    }

    #[test]
    fn a_moved_keeper_holding_another_child_or_session_refuses() {
        let wrong_child = verify_moved(
            &plan(),
            &identify(9999, "11111111-2222-3333-4444-555555555555", 900),
        )
        .expect_err("another child refuses");
        assert!(wrong_child.contains("9999"), "{wrong_child}");
        assert!(wrong_child.contains("4242"), "{wrong_child}");

        let wrong_session =
            verify_moved(&plan(), &identify(4242, "another-session", 900)).expect_err("refuses");
        assert!(wrong_session.contains("another-session"), "{wrong_session}");
    }

    #[test]
    fn a_moved_keeper_answering_nothing_refuses_rather_than_guessing() {
        let silent = verify_moved(&plan(), &serde_json::json!({})).expect_err("refuses");
        assert!(silent.contains("child pid"), "{silent}");

        let no_session = verify_moved(
            &plan(),
            &serde_json::json!({"keeper_pid": 900, "child_pid": 4242, "argv": ["pi"]}),
        )
        .expect_err("refuses");
        assert!(no_session.contains("session id"), "{no_session}");
    }

    #[test]
    fn the_flipped_row_reads_as_a_keeper_thread() {
        let mut entry = crate::state::RegistryEntry {
            name: "king-delivery".to_string(),
            cwd: "/repo".to_string(),
            status: crate::AgentStatus::Live,
            created_at: "2026-09-20T00:00:00Z".to_string(),
            ..Default::default()
        };
        entry.substrate = Some("pane".to_string());
        entry.mux = Some(crate::state::MuxRef {
            session: "fno".to_string(),
            pane_id: 2313,
        });
        entry.pid = Some(4242);
        flipped_row(
            &mut entry,
            &RebindOutcome {
                socket: "/state/mux/threads/king-delivery.sock".to_string(),
                keeper_pid: 900,
                child_pid: 4242,
                session_id: "sid".to_string(),
            },
        );
        assert_eq!(entry.substrate.as_deref(), Some("thread"));
        assert!(entry.mux.is_none(), "a thread row holds no mux ref");
        assert_eq!(
            entry.messaging_socket_path.as_deref(),
            Some("/state/mux/threads/king-delivery.sock")
        );
        // The pid pair is the one a reader can get backwards, so it is
        // asserted both ways round.
        assert_eq!(entry.pid, Some(900), "pid is the KEEPER on a thread row");
        assert_eq!(entry.keeper_child_pid, Some(4242));
    }
}
