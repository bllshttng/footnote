//! The registry-side keeper sweep test family (x-ac6b), moved verbatim out of
//! daemon.rs for file budget: test motion is the sanctioned shrink.

use super::*;

// ------------------------------------------------------------------------
// Registry-side keeper sweep (x-ac6b). A FAKE keeper answering Identify is
// enough here: the real supervisor kill is the last group's journey and
// this plan does not claim it.
// ------------------------------------------------------------------------

/// An agents home under a UNIQUE short base dir, so `mux/threads/` (the
/// sweep's directory, derived from the agents root's parent) never collides
/// between parallel tests the way a shared `/tmp/mux` would.
pub(super) fn keeper_sweep_home(tag: &str) -> AgentsHome {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    let n = C.fetch_add(1, Ordering::Relaxed);
    let base = PathBuf::from(format!("/tmp/fnokswp{tag}{}_{n}", std::process::id()));
    // Pids recycle, so a prior run may own this path; a stale socket file
    // in it fails fixture binds with EADDRINUSE. Start from an empty base.
    let _ = std::fs::remove_dir_all(&base);
    let home = AgentsHome::at(base.join("agents"));
    home.ensure_root().unwrap();
    std::fs::create_dir_all(lane_b_keeper_dir(&home)).unwrap();
    home
}

/// A registry row shaped exactly like the lane-B spawn writes it (pi
/// harness, interactive, socket-keyed, session id minted before launch).
fn lane_b_thread_row(
    name: &str,
    session: &str,
    cwd: &str,
    child_pid: Option<u32>,
    sock: &Path,
) -> RegistryEntry {
    RegistryEntry {
        substrate: None,
        node: None,
        spawned_by_session: None,
        spawned_by_harness: None,
        spawned_by_cwd: None,
        launch_account: None,
        related_session_id: None,
        origin: Some("spawn".into()),
        name: name.into(),
        short_id: String::new(),
        legacy_provider: String::new(),
        provider: None,
        model: None,
        model_basis: None,
        effort: None,
        harness: Some("pi".into()),
        harness_session_id: Some(session.into()),
        predecessor_session_ids: Vec::new(),
        forked_from_session_id: None,
        cwd: cwd.into(),
        project_root: String::new(),
        session_id: None,
        spawn_trigger: None,
        legacy_claude_short_id: None,
        claude_session_uuid: None,
        // The keeper's own pid: NOT the child pid, which rides
        // keeper_child_pid.
        pid: Some(4242),
        pid_start_time: None,
        keeper_child_pid: child_pid,
        messaging_socket_path: Some(sock.to_string_lossy().into_owned()),
        codex_session_id: None,
        gemini_session_id: None,
        mcp_channel_id: None,
        cc_session_id: None,
        host_mode: Some("interactive".into()),
        status: AgentStatus::Live,
        last_message_at: None,
        created_at: "2026-09-01T00:00:00Z".into(),
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
    }
}

/// A fake keeper behind `sock`, speaking the real frame protocol via the
/// binary's own codec. `reply` is the IdentifyReply JSON it answers with.
/// Exits after serving one Identify.
fn spawn_fake_keeper(sock: &Path, reply: serde_json::Value) -> std::thread::JoinHandle<()> {
    use std::os::unix::net::UnixListener;
    std::fs::create_dir_all(sock.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(sock).unwrap();
    std::thread::Builder::new()
        .name("fake-keeper".into())
        .spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            serve_fake_identify(&mut stream, &reply);
        })
        .unwrap()
}

fn serve_fake_identify(stream: &mut std::os::unix::net::UnixStream, reply: &serde_json::Value) {
    use crate::pane_keeper::{decode, encode, Decode, Frame};
    use std::io::{Read, Write};
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        loop {
            match decode(&buf) {
                Decode::NeedMore => break,
                Decode::Violation(_) => return,
                Decode::Frame(Frame::Identify, _) => {
                    let frame = encode(&Frame::IdentifyReply(reply.to_string().into_bytes()));
                    let _ = stream.write_all(&frame);
                    let _ = stream.flush();
                    // Hold the connection a beat so the probe's reply read
                    // is not EOF-raced.
                    std::thread::sleep(Duration::from_millis(100));
                    return;
                }
                Decode::Frame(_, used) => {
                    buf.drain(..used);
                }
            }
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// The AC4 wedged shape: accepts the connection and never answers. Parks
/// until `stop` flips so the test can end the thread deterministically.
fn spawn_silent_keeper(
    sock: &Path,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    std::fs::create_dir_all(sock.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(sock).unwrap();
    std::thread::Builder::new()
        .name("silent-keeper".into())
        .spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            // Consume the Identify frame, answer nothing.
            let mut chunk = [0u8; 64];
            let _ = stream.read(&mut chunk);
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(25));
            }
        })
        .unwrap()
}

/// A keeper that honors the Kill contract: on the Kill frame it unlinks
/// its socket and stops serving, the way the real keeper exits after
/// SIGKILLing its child (pane_keeper.rs). Parks until `stop` flips so the
/// test can end the thread deterministically even on refusal paths.
fn spawn_killable_keeper(
    sock: &Path,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    use crate::pane_keeper::{decode, Decode, Frame};
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::Ordering::SeqCst;
    std::fs::create_dir_all(sock.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(sock).unwrap();
    let sock_path = sock.to_path_buf();
    std::thread::Builder::new()
        .name("killable-keeper".into())
        .spawn(move || {
            listener.set_nonblocking(true).unwrap();
            while !stop.load(SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        let mut buf: Vec<u8> = Vec::new();
                        let mut chunk = [0u8; 4096];
                        loop {
                            match decode(&buf) {
                                Decode::NeedMore => {}
                                Decode::Violation(_) => return,
                                Decode::Frame(frame, used) => {
                                    buf.drain(..used);
                                    if matches!(frame, Frame::Kill) {
                                        // The real keeper kills the child
                                        // here; there is no child to kill
                                        // behind the fake.
                                        let _ = std::fs::remove_file(&sock_path);
                                        stop.store(true, SeqCst);
                                        return;
                                    }
                                }
                            }
                            match stream.read(&mut chunk) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            }
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => return,
                }
            }
            let _ = std::fs::remove_file(&sock_path);
        })
        .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn keeper_thread_stop_confirms_the_kill_and_stamps_the_row_exited() {
    // PR 1332 review finding: a lane-B row's empty short_id fell into the
    // no-op arm, which reported a stop that stopped nothing. The keeper
    // arm must Kill over the row's own socket, CONFIRM the keeper went
    // away, and only then stamp the row terminal.
    //
    // x-9c91: the stop now releases the stopped row's claims, which resolves
    // the row-cwd space dir, so the hermetic guard needs its pin.
    let home = keeper_sweep_home("kpstop");
    // The env pins race every other test that resolves state roots; hold the
    // shared lock for their whole lifetime.
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::env::set_var("FNO_SPACES_DIR", home.root().join("spaces"));
    std::env::set_var("FNO_CLAIMS_ROOT", home.root().join("claims-root"));
    let sock = lane_b_keeper_dir(&home).join("wk-stop.sock");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let keeper = spawn_killable_keeper(&sock, Arc::clone(&stop));
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(lane_b_thread_row(
            "wk-stop",
            "sess-1",
            "/repo",
            Some(555),
            &sock,
        ));
    })
    .unwrap();
    let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));

    let response = handle_stop(
        &ctx,
        &Request::new(1, "agent.stop", json!({"name": "wk-stop"})),
    )
    .await;
    let result = response.result().expect("stop errored");
    assert_eq!(result["stopped"], true, "{result:?}");
    assert_eq!(result["backend"], "keeper-thread", "{result:?}");

    keeper.join().unwrap();
    assert!(!sock.exists(), "the keeper unlinks its own socket on Kill");
    let registry = load_registry_offloaded(home.registry_json()).await.unwrap();
    let entry = registry.find("wk-stop").unwrap();
    assert_eq!(entry.status, AgentStatus::Exited);
    assert!(
        entry.exited_at.is_some(),
        "the terminal stamp carries a time"
    );
    assert!(read_events(&home).iter().any(|event| {
        event.get("type").and_then(Value::as_str) == Some("agent_stopped")
            && event
                .get("data")
                .and_then(|data| data.get("backend"))
                .and_then(Value::as_str)
                == Some("keeper-thread")
    }));
    std::env::remove_var("FNO_SPACES_DIR");
    std::env::remove_var("FNO_CLAIMS_ROOT");
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test(flavor = "current_thread")]
async fn keeper_thread_stop_refuses_when_the_keeper_never_confirms() {
    // A keeper that swallows the Kill frame leaves the row non-terminal:
    // reporting a stop over a live keeper is the zombie shape.
    let home = keeper_sweep_home("kprefu");
    let sock = lane_b_keeper_dir(&home).join("wk-stubborn.sock");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _keeper = spawn_silent_keeper(&sock, Arc::clone(&stop));
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(lane_b_thread_row(
            "wk-stubborn",
            "sess-2",
            "/repo",
            Some(555),
            &sock,
        ));
    })
    .unwrap();
    let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));

    let response = handle_stop(
        &ctx,
        &Request::new(2, "agent.stop", json!({"name": "wk-stubborn"})),
    )
    .await;
    assert!(
        response.result().is_none(),
        "an unconfirmed keeper must error, not report a stop"
    );
    let registry = load_registry_offloaded(home.registry_json()).await.unwrap();
    assert_ne!(
        registry.find("wk-stubborn").map(|entry| entry.status),
        Some(AgentStatus::Exited),
        "a refused stop must not stamp the row terminal"
    );
    assert!(sock.exists(), "a refused stop never unlinks the socket");
    assert!(read_events(&home)
        .iter()
        .any(|event| { event.get("type").and_then(Value::as_str) == Some("agent_stop_refused") }));
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test(flavor = "current_thread")]
async fn stop_worker_confirmed_routes_a_keeper_row_to_its_own_socket() {
    // The forced-rm orphan: a lane-B row's empty short_id derived
    // worker_sock(""), and the probe over that absent socket confirmed a
    // stop over a socket the keeper does not own. The delegation must
    // Kill the row's OWN socket.
    let home = keeper_sweep_home("kprm");
    let sock = lane_b_keeper_dir(&home).join("wk-rm.sock");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let keeper = spawn_killable_keeper(&sock, Arc::clone(&stop));
    let entry = lane_b_thread_row("wk-rm", "sess-3", "/repo", Some(555), &sock);
    let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));

    let confirmed = stop_worker_confirmed(&ctx, &entry).await;
    keeper.join().unwrap();
    assert!(confirmed, "a Kill-honoring keeper confirms down");
    assert!(!sock.exists());
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn keeper_sweep_apply_discards_a_verdict_when_the_row_changed_identity() {
    // The P1 race: the sweep snapshots the registry, probes up to the
    // budget while the daemon already serves, and an operator removes the
    // row and re-spawns under the SAME name before the write. A name-only
    // apply would stamp the old keeper's verdict onto the healthy
    // replacement.
    let mut registry = state::Registry::default();
    registry.entries.push(lane_b_thread_row(
        "wk-raced",
        "sess-new",
        "/tmp",
        None,
        Path::new("/tmp/mux/threads/wk-raced.sock"),
    ));
    let changes = vec![KeeperSweepChange {
        name: "wk-raced".into(),
        status: Some(AgentStatus::Exited),
        child_pid: None,
        bound_socket: Some("/tmp/mux/threads/wk-raced.sock".into()),
        bound_session: Some("sess-old".into()),
    }];
    let superseded = apply_keeper_sweep_changes(&mut registry, &changes, "2026-09-01T00:00:00Z");
    assert_eq!(
        superseded,
        vec!["wk-raced"],
        "the raced row is named superseded"
    );
    assert_eq!(
        registry.entries[0].status,
        AgentStatus::Live,
        "the replacement row keeps its own status"
    );
    // The same change against the row it was probed from still applies.
    let mut matching = state::Registry::default();
    matching.entries.push(lane_b_thread_row(
        "wk-raced",
        "sess-old",
        "/tmp",
        None,
        Path::new("/tmp/mux/threads/wk-raced.sock"),
    ));
    let superseded = apply_keeper_sweep_changes(&mut matching, &changes, "2026-09-01T00:00:00Z");
    assert!(superseded.is_empty(), "identity-held change applies");
    assert_eq!(matching.entries[0].status, AgentStatus::Exited);
}

#[test]
fn keeper_registry_sweep_flags_a_moved_cwd_even_when_the_child_pid_matches() {
    // The P2 chain bug: an else-if identity ladder skips the cwd leg
    // whenever both child pids are present and equal, re-binding a keeper
    // that answers from a different directory.
    let home = keeper_sweep_home("cwd");
    let threads = lane_b_keeper_dir(&home);
    let recorded_cwd = home.root().parent().unwrap().to_string_lossy().into_owned();
    let elsewhere = home.root().to_string_lossy().into_owned();
    let sock = threads.join("wk-moved.sock");
    let keeper = spawn_fake_keeper(
        &sock,
        json!({
            "v": 1, "keeper_pid": 4242, "child_pid": 111,
            "session_id": "sess-moved", "cwd": elsewhere, "argv": [],
        }),
    );
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(lane_b_thread_row(
            "wk-moved",
            "sess-moved",
            &recorded_cwd,
            Some(111),
            &sock,
        ));
    })
    .unwrap();

    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let report = keeper_registry_sweep(&home, &emitter).expect("sweep ok");
    assert!(report.rebound.is_empty(), "a moved cwd must not re-bind");
    assert_eq!(report.dead.len(), 1, "the moved cwd is named dead");
    assert!(
        report.dead[0].1.contains("cwd"),
        "the reason names the cwd mismatch: {}",
        report.dead[0].1
    );
    let row = state::load_registry(&home.registry_json())
        .unwrap()
        .entries
        .remove(0);
    assert_eq!(row.status, AgentStatus::Exited);
    keeper.join().unwrap();
    std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
}

#[test]
fn keeper_registry_sweep_names_every_outcome_and_bounds_the_wedged_probe() {
    let home = keeper_sweep_home("all");
    let threads = lane_b_keeper_dir(&home);
    let cwd = home.root().parent().unwrap().to_string_lossy().into_owned();
    let sock = |name: &str| threads.join(format!("{name}.sock"));

    // Live: the keeper answers the row's own identity.
    let live = spawn_fake_keeper(
        &sock("wk-live"),
        json!({
            "v": 1, "keeper_pid": 4242, "child_pid": 111,
            "session_id": "sess-live", "cwd": cwd, "argv": ["pi", "--session-id", "sess-live"],
        }),
    );
    // Clone: a keeper answering a DIFFERENT session id under the row's
    // socket - the respawn-wearing-the-name failure (AC3-ERR).
    let clone = spawn_fake_keeper(
        &sock("wk-clone"),
        json!({"v": 1, "keeper_pid": 5, "child_pid": 6, "session_id": "sess-other", "cwd": cwd}),
    );
    // Respawn: same session id, DIFFERENT child pid. Passes any liveness
    // check and must still fail this one - that is the point.
    let respawn = spawn_fake_keeper(
        &sock("wk-respawn"),
        json!({"v": 1, "keeper_pid": 7, "child_pid": 999, "session_id": "sess-respawn", "cwd": cwd}),
    );
    // Wedged: accepts, never answers (AC4-ERR).
    let wedge_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let wedge = spawn_silent_keeper(&sock("wk-wedge"), Arc::clone(&wedge_stop));
    // Dead: a socket file with no listener behind it.
    std::fs::write(sock("wk-dead"), b"").unwrap();

    let rows = [
        lane_b_thread_row("wk-live", "sess-live", &cwd, Some(111), &sock("wk-live")),
        lane_b_thread_row("wk-clone", "sess-clone", &cwd, Some(222), &sock("wk-clone")),
        lane_b_thread_row(
            "wk-respawn",
            "sess-respawn",
            &cwd,
            Some(111),
            &sock("wk-respawn"),
        ),
        lane_b_thread_row("wk-wedge", "sess-wedge", &cwd, Some(333), &sock("wk-wedge")),
        lane_b_thread_row("wk-dead", "sess-dead", &cwd, Some(444), &sock("wk-dead")),
    ];
    state::update_registry(&home.registry_json(), |r| {
        r.entries.extend(rows.iter().cloned());
    })
    .unwrap();

    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let start = Instant::now();
    let report = keeper_registry_sweep(&home, &emitter).expect("sweep ok");
    let elapsed = start.elapsed();

    // All five sockets examined; exactly one re-bound.
    assert_eq!(report.sockets, 5);
    assert_eq!(
        report.rebound,
        vec!["wk-live"],
        "only the live keeper re-binds"
    );
    let dead_names: Vec<&str> = report.dead.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        dead_names,
        vec!["wk-clone", "wk-dead", "wk-respawn"],
        "every dead row is named, sorted by socket"
    );
    let clone_reason = &report.dead[0].1;
    assert!(
        clone_reason.contains("session id") && clone_reason.contains("sess-other"),
        "the clone's reason names the session mismatch: {clone_reason}"
    );
    assert!(
        report.dead[2].1.contains("child pid changed"),
        "the respawn's reason names the pid change: {}",
        report.dead[2].1
    );
    assert_eq!(report.wedged[0].0, "wk-wedge", "the wedged row is named");
    assert!(
        report.wedged[0].1.contains("did not answer"),
        "the wedged reason names the silence: {}",
        report.wedged[0].1
    );
    // The stale socket is unlinked; live listeners are never unlinked.
    assert_eq!(
        report.unlinked,
        vec![sock("wk-dead").to_string_lossy().into_owned()]
    );
    assert!(sock("wk-clone").exists(), "a live keeper's socket stays");
    assert!(sock("wk-wedge").exists(), "a wedged keeper's socket stays");
    // Bounded: one wedged probe costs one reply timeout, not a hang.
    assert!(
        elapsed < Duration::from_secs(5),
        "the sweep completed inside its budget, took {elapsed:?}"
    );

    let registry = state::load_registry(&home.registry_json()).unwrap();
    let row = |name: &str| {
        registry
            .entries
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("{name} row"))
            .clone()
    };
    assert_eq!(row("wk-live").status, AgentStatus::Live);
    assert_eq!(row("wk-live").keeper_child_pid, Some(111));
    assert_eq!(row("wk-clone").status, AgentStatus::Exited);
    assert_eq!(row("wk-respawn").status, AgentStatus::Exited);
    // The recorded child pid survives the dead verdict as forensics.
    assert_eq!(row("wk-respawn").keeper_child_pid, Some(111));
    assert_eq!(row("wk-dead").status, AgentStatus::Exited);
    assert_eq!(
        row("wk-dead").pid,
        None,
        "Exited clears the stale pid (Locked 7)"
    );
    // Silence never proves death: the wedged row is untouched.
    assert_eq!(row("wk-wedge").status, AgentStatus::Live);

    // End the fake keepers before teardown.
    wedge_stop.store(true, std::sync::atomic::Ordering::SeqCst);
    for handle in [live, clone, respawn, wedge] {
        handle.join().unwrap();
    }
    std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
}

#[test]
fn keeper_reattach_identity_asserts_cwd_session_and_child_pid_across_the_restart() {
    let home = keeper_sweep_home("id");
    let threads = lane_b_keeper_dir(&home);
    let cwd = home.root().parent().unwrap().to_string_lossy().into_owned();
    // A child pid that is PROVABLY alive: this test process. The keeper
    // answers it, the row records it, and the assertion that the exact
    // pid survived the restart is a real signal(0), not a string compare.
    let child_pid = std::process::id();
    let sock = threads.join("wk-pi.sock");
    let keeper = spawn_fake_keeper(
        &sock,
        json!({
            "v": 1, "keeper_pid": 4242, "child_pid": child_pid,
            "session_id": "sess-pi", "cwd": cwd, "argv": ["pi", "--session-id", "sess-pi"],
        }),
    );
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(lane_b_thread_row(
            "wk-pi",
            "sess-pi",
            &cwd,
            Some(child_pid),
            &sock,
        ));
    })
    .unwrap();

    // The restart: the daemon died and came back, and its startup sweep is
    // the only thing that walks the socket back to the row.
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let report = keeper_registry_sweep(&home, &emitter).expect("sweep ok");
    assert_eq!(report.rebound, vec!["wk-pi"]);
    assert!(report.dead.is_empty() && report.wedged.is_empty());

    let row = state::load_registry(&home.registry_json())
        .unwrap()
        .entries
        .into_iter()
        .find(|e| e.name == "wk-pi")
        .unwrap();
    // Byte-equal identity across the restart, and nothing new minted.
    assert_eq!(row.status, AgentStatus::Live, "the row is live again");
    assert_eq!(row.harness_session_id.as_deref(), Some("sess-pi"));
    assert_eq!(row.cwd, cwd, "cwd is unchanged");
    assert_eq!(
        row.keeper_child_pid,
        Some(child_pid),
        "the child pid is unchanged"
    );
    assert_eq!(row.pid, Some(4242), "the keeper pid field is untouched");
    // The exact pid is still alive - same child, not a respawn wearing it.
    // SAFETY: signal 0 against this process's own pid is a pure liveness
    // probe; it delivers no signal.
    assert_eq!(unsafe { libc::kill(child_pid as libc::pid_t, 0) }, 0);

    keeper.join().unwrap();
    std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
}
