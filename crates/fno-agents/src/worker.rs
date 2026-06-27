//! Per-agent worker shim (Wave 3, task 3.3 — Outcome B).
//!
//! Wave 0 proved that a child on a PTY whose master the *daemon* owns is
//! SIGHUP'd and dies the instant the daemon closes the master. The locked fix
//! (Outcome B) is this: a per-agent worker process owns the PTY master and
//! **outlives the daemon**. The daemon spawns one worker per PTY-managed agent,
//! puts it in its own process group (so a kill of the daemon's group does not
//! reach it), and talks to it over `<short_id>/worker.sock`. On daemon restart,
//! the recovery sweep rediscovers live workers by scanning for their sockets
//! and reattaches.
//!
//! The worker is intentionally tiny and single-client: only the daemon connects
//! to it, so requests are handled serially on a current-thread runtime. There
//! is no `Send` requirement (the [`PtySession`] is never moved across tasks),
//! which sidesteps the `MasterPty: !Sync` constraint cleanly.

use crate::events::EventEmitter;
use crate::paths::{self, AgentsHome};
use crate::protocol::{read_request, write_response, ErrorCode, ProtocolError, Request, Response};
use crate::pty::{PtySession, DEFAULT_OUTPUT_RING_BYTES};
use crate::state::{self, AgentState};
use crate::AgentStatus;
use base64::Engine as _;
use portable_pty::CommandBuilder;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};

/// How the daemon launches a worker: provider argv + where + terminal size.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub short_id: String,
    pub home: PathBuf,
    pub cwd: PathBuf,
    /// Provider command line: `argv[0]` is the program, the rest its args.
    pub argv: Vec<String>,
    pub rows: u16,
    pub cols: u16,
    pub ring_bytes: usize,
}

impl WorkerConfig {
    /// Build from the worker binary's parsed args, defaulting the ring size.
    pub fn new(
        short_id: impl Into<String>,
        home: impl Into<PathBuf>,
        cwd: impl Into<PathBuf>,
        argv: Vec<String>,
    ) -> Self {
        WorkerConfig {
            short_id: short_id.into(),
            home: home.into(),
            cwd: cwd.into(),
            argv,
            rows: 24,
            cols: 80,
            ring_bytes: DEFAULT_OUTPUT_RING_BYTES,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("worker config: no provider argv given")]
    NoArgv,
    #[error("pty: {0}")]
    Pty(#[from] crate::pty::PtyError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("state: {0}")]
    State(#[from] state::StateError),
}

/// Run the worker: spawn the PTY child, publish `state.json`, bind
/// `worker.sock`, and serve daemon RPCs until the child exits or a
/// `worker.shutdown` arrives. Returns when the worker should exit.
pub async fn run(cfg: WorkerConfig) -> Result<(), WorkerError> {
    if cfg.argv.is_empty() {
        return Err(WorkerError::NoArgv);
    }
    let home = AgentsHome::at(&cfg.home);
    let sock_path = home.worker_sock(&cfg.short_id);
    let state_path = home.state_json(&cfg.short_id);

    // Spawn the PTY child the worker will own for its whole lifetime.
    let cmd = build_child_command(&cfg);
    let pty = PtySession::spawn(cmd, cfg.rows, cfg.cols, cfg.ring_bytes)?;

    // Publish live state (status=live, pty.active=true) so the daemon and the
    // daemon-down read path both see a coherent picture.
    let mut st = AgentState::new_pty(&cfg.short_id);
    st.status = AgentStatus::Live;
    st.ready = true;
    if let Some(p) = st.pty.as_mut() {
        p.active = true;
    }
    state::write_state_atomic(&state_path, &st)?;

    // Bind the worker socket (replace any stale socket from a prior incarnation)
    // and lock it to mode 0600.
    let _ = std::fs::remove_file(&sock_path);
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&sock_path)?;
    let _ = paths::set_file_mode_0600(&sock_path);

    let mut liveness = tokio::time::interval(Duration::from_millis(250));
    liveness.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        if serve_connection(&pty, stream).await {
                            // shutdown requested
                            break;
                        }
                    }
                    Err(_) => continue,
                }
            }
            _ = liveness.tick() => {
                if !pty.is_child_alive() {
                    break;
                }
            }
        }
    }

    // Child exited or shutdown requested. Emit an operator-visible exit event
    // (so events.jsonl is not silent on steady-state agent death), flip the
    // registry row to Exited (so `agent.list` does not keep reporting Live
    // until the next daemon restart/reconcile), mark state.json, drop the
    // socket, and exit. (silent-failure #1 + #2.)
    let child_alive_at_exit = pty.is_child_alive();
    let emitter = EventEmitter::new(home.events_jsonl(), format!("worker:{}", cfg.short_id));
    let _ = emitter.emit(
        "agent_exited",
        &serde_json::json!({
            "short_id": cfg.short_id,
            "reason": if child_alive_at_exit { "shutdown" } else { "child_exited" },
        }),
    );
    if let Err(e) = state::update_registry(&home.registry_json(), |r| {
        if let Some(entry) = r.entries.iter_mut().find(|e| e.short_id == cfg.short_id) {
            entry.status = AgentStatus::Exited;
        }
    }) {
        eprintln!(
            "fno-agents-worker: registry exit-update failed for {}: {e}",
            cfg.short_id
        );
    }
    if let Err(e) = finalize(&pty, &state_path, &cfg.short_id) {
        eprintln!(
            "fno-agents-worker: state.json exit-write failed for {}: {e}",
            cfg.short_id
        );
    }
    let _ = std::fs::remove_file(&sock_path);
    Ok(())
}

/// Serve requests on one daemon connection until EOF or `worker.shutdown`.
/// Returns `true` if shutdown was requested (the run loop then exits).
async fn serve_connection(pty: &PtySession, mut stream: UnixStream) -> bool {
    loop {
        let req = match read_request(&mut stream).await {
            Ok(r) => r,
            // Clean hangup or any read fault: end this connection, keep serving.
            Err(ProtocolError::UnexpectedEof) | Err(_) => return false,
        };
        let (resp, shutdown) = handle(pty, &req);
        if write_response(&mut stream, &resp).await.is_err() {
            return shutdown;
        }
        if shutdown {
            return true;
        }
    }
}

/// Handle one worker RPC. Returns the response and whether shutdown was asked.
fn handle(pty: &PtySession, req: &Request) -> (Response, bool) {
    match req.method.as_str() {
        "worker.ping" => (Response::ok(req.id, json!({"pong": true})), false),
        "worker.write" => {
            // Two input shapes: `data` (a UTF-8 string, the ask path) or
            // `bytes_b64` (base64 of raw keystroke bytes, the drive path, which
            // must carry control chars and arbitrary non-UTF-8 bytes a JSON
            // string cannot). Exactly one is expected; `bytes_b64` wins if both
            // are present.
            let bytes: Option<Vec<u8>> = match req.params.get("bytes_b64").and_then(|v| v.as_str())
            {
                Some(b64) => match base64::engine::general_purpose::STANDARD.decode(b64) {
                    Ok(raw) => Some(raw),
                    Err(e) => {
                        return (
                            Response::err(
                                req.id,
                                ErrorCode::InvalidParams,
                                format!("invalid base64 in `bytes_b64`: {e}"),
                            ),
                            false,
                        )
                    }
                },
                None => req
                    .params
                    .get("data")
                    .and_then(|v| v.as_str())
                    .map(|s| s.as_bytes().to_vec()),
            };
            match bytes {
                Some(raw) => match pty.write_input(&raw) {
                    Ok(()) => (Response::ok(req.id, json!({"written": raw.len()})), false),
                    Err(e) => (
                        Response::err(req.id, ErrorCode::Internal, format!("write failed: {e}")),
                        false,
                    ),
                },
                None => (
                    Response::err(
                        req.id,
                        ErrorCode::InvalidParams,
                        "missing `data` (string) or `bytes_b64` (base64)",
                    ),
                    false,
                ),
            }
        }
        "worker.read_since" => {
            // Incremental PTY-output read for drive streaming. `cursor` is the
            // absolute byte offset returned by the prior call (0 for a fresh
            // reader); the response carries the new bytes (base64), the next
            // cursor, whether a gap (dropped bytes) preceded this read, and
            // child liveness so the drive pump can detect exit.
            let cursor = req
                .params
                .get("cursor")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let rs = pty.read_since(cursor);
            let b64 = base64::engine::general_purpose::STANDARD.encode(&rs.bytes);
            (
                Response::ok(
                    req.id,
                    json!({
                        "bytes_b64": b64,
                        "next_offset": rs.next,
                        "gap": rs.gap,
                        "child_alive": pty.is_child_alive(),
                    }),
                ),
                false,
            )
        }
        "worker.snapshot" => {
            let snap = pty.snapshot();
            let text = String::from_utf8_lossy(&snap).into_owned();
            (
                Response::ok(
                    req.id,
                    json!({
                        "text": text,
                        "dropped_bytes": pty.dropped_bytes(),
                        "child_alive": pty.is_child_alive(),
                    }),
                ),
                false,
            )
        }
        "worker.status" => (
            Response::ok(
                req.id,
                json!({
                    "child_pid": pty.child_pid(),
                    "child_alive": pty.is_child_alive(),
                    "drain_outcome": format!("{:?}", pty.drain_outcome()),
                }),
            ),
            false,
        ),
        "worker.resize" => {
            let rows = req.params.get("rows").and_then(|v| v.as_u64());
            let cols = req.params.get("cols").and_then(|v| v.as_u64());
            match (rows, cols) {
                (Some(r), Some(c)) => match pty.resize(r as u16, c as u16) {
                    Ok(()) => (Response::ok(req.id, json!({"resized": true})), false),
                    Err(e) => (
                        Response::err(req.id, ErrorCode::Internal, format!("resize: {e}")),
                        false,
                    ),
                },
                _ => (
                    Response::err(req.id, ErrorCode::InvalidParams, "need rows and cols"),
                    false,
                ),
            }
        }
        "worker.shutdown" => {
            let _ = pty.kill();
            (Response::ok(req.id, json!({"shutdown": true})), true)
        }
        other => (
            Response::err(
                req.id,
                ErrorCode::UnknownMethod,
                format!("unknown worker method: {other}"),
            ),
            false,
        ),
    }
}

/// Best-effort terminal state write on exit. A failure here is logged via the
/// return path's caller, never fatal — the worker is exiting regardless.
/// Terminal state write on exit. Returns the write result so `run()` can log a
/// failure rather than discarding it (the prior version's doc claimed
/// caller-side logging that did not exist — now it does).
fn finalize(
    pty: &PtySession,
    state_path: &std::path::Path,
    short_id: &str,
) -> Result<(), state::StateError> {
    let mut st = state::load_state(state_path)
        .ok()
        .flatten()
        .unwrap_or_else(|| AgentState::new_pty(short_id));
    st.status = AgentStatus::Exited;
    st.ready = false;
    if let Some(p) = st.pty.as_mut() {
        p.active = false;
    }
    let _ = pty.kill();
    state::write_state_atomic(state_path, &st)
}

/// Build the PTY child command from a [`WorkerConfig`], stamping the
/// drive-authority identity variables into its environment.
///
/// Extracted into a dedicated function so the stamp contract is unit-testable
/// without spawning a PTY (ab-1e86b88e: locks the drive-authority LD3 identity
/// stamp read by scripts/lib/drive-authority.sh against silent removal).
fn build_child_command(cfg: &WorkerConfig) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(&cfg.argv[0]);
    for a in &cfg.argv[1..] {
        cmd.arg(a);
    }
    cmd.cwd(&cfg.cwd);
    // Stamp this agent's identity into the PTY child's environment (cv-140f09c3).
    // The child (claude/codex) and any Stop / graph-write-protect hook it spawns
    // inherit FNO_AGENTS_SELF_SHORT_ID, so the drive-authority guard can scope
    // itself to THIS agent: it fires only when an open operator drive window
    // targets this short_id, never on a window driving some unrelated agent.
    cmd.env("FNO_AGENTS_SELF_SHORT_ID", &cfg.short_id);
    cmd.env("FNO_AGENTS_HOME", cfg.home.as_os_str());
    // Interactive-claude persistence recipe (inside-out-multiplexer E4.1): the
    // daemon-side equivalent of the relay's roundtrip.py `_peer_env`. Force the
    // PTY child to write its OWN faithful transcript at
    // projects/<cwd-enc>/<session-id>.jsonl - which the relay then globs+reads
    // with no PTY of its own (AC-E4-1) - by overriding the child's
    // persistence-skip and dropping any inherited parent session id that would
    // cross-write the child's turns into the parent's transcript. The child's own
    // id is pinned via `--session-id` in the argv. Gated to interactive claude so
    // codex/gemini panes (and the daemon's own env) are byte-unchanged.
    if is_interactive_claude(&cfg.argv) {
        cmd.env("CLAUDE_CODE_FORCE_SESSION_PERSISTENCE", "1");
        cmd.env_remove("CLAUDE_CODE_SESSION_ID");
    }
    cmd
}

/// True when this PTY child is interactive subscription-billed claude (E4.1): the
/// program is `claude` and no `-p`/`--print` Agent-SDK flag is present. Mirrors
/// the daemon's `claude_argv_is_interactive` billing guard, kept local so the
/// recipe is self-contained in the worker.
fn is_interactive_claude(argv: &[String]) -> bool {
    argv.first()
        .and_then(|p| Path::new(p).file_name())
        .map(|f| f == "claude")
        .unwrap_or(false)
        && !argv.iter().any(|a| a == "-p" || a == "--print")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{read_response, write_request};
    use std::ffi::OsStr;
    use std::time::Instant;

    // Serializes the env-mutating tests below: lib tests run concurrently in one
    // process and every `CommandBuilder::new()` reads `std::env::vars_os()`, so a
    // bare `set_var` here would race a concurrent read. Same idiom as
    // `agents_config::tests::ENV_LOCK` / `provider::tests::HOME_LOCK`.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // --- ab-1e86b88e: drive-authority LD3 identity stamp regression tests ----
    //
    // These tests lock the contract that build_child_command stamps BOTH
    // FNO_AGENTS_SELF_SHORT_ID (the per-agent identity read by
    // scripts/lib/drive-authority.sh) and FNO_AGENTS_HOME into every PTY child's
    // environment.  The stamp must never be silently dropped: a child process
    // (claude/codex) and every Stop / graph-write-protect hook it spawns inherit
    // the variable so the drive-authority guard can scope itself to THIS agent.
    // End-to-end propagation (worker -> PTY child -> bash Stop hook) was verified
    // live on 2026-05-31 against Claude Code 2.1.156; see
    // tests/hooks/verify-self-short-id-propagation.sh for the manual verifier.

    #[test]
    fn build_child_command_stamps_self_short_id() {
        // ab-1e86b88e: locks FNO_AGENTS_SELF_SHORT_ID stamp in build_child_command.
        // If this test starts failing it means the drive-authority LD3 identity
        // guard (drive-authority.sh) will no longer be able to scope itself to
        // the correct agent - do NOT remove this assertion.
        let cfg = WorkerConfig::new(
            "wk-1a2b3c",
            PathBuf::from("/tmp/abi-test-home"),
            PathBuf::from("/tmp"),
            vec!["claude".to_string()],
        );
        let cmd = build_child_command(&cfg);
        assert_eq!(
            cmd.get_env("FNO_AGENTS_SELF_SHORT_ID"),
            Some(OsStr::new("wk-1a2b3c")),
            "FNO_AGENTS_SELF_SHORT_ID must be stamped with the worker's short_id \
             (read by scripts/lib/drive-authority.sh for LD3 scope guard)"
        );
    }

    #[test]
    fn build_child_command_stamps_fno_agents_home() {
        // ab-1e86b88e: companion to the short_id test - home must also be stamped
        // so the child and its hooks can locate the shared agents store.
        let home = PathBuf::from("/tmp/abi-test-home-2");
        let cfg = WorkerConfig::new(
            "wk-deadbeef",
            home.clone(),
            PathBuf::from("/tmp"),
            vec!["codex".to_string()],
        );
        let cmd = build_child_command(&cfg);
        assert_eq!(
            cmd.get_env("FNO_AGENTS_HOME"),
            Some(home.as_os_str()),
            "FNO_AGENTS_HOME must be stamped with the worker's home path"
        );
    }

    #[test]
    fn build_child_command_applies_claude_persistence_recipe() {
        // E4.1: interactive claude gets the persistence recipe so it writes its
        // own transcript jsonl (the relay's AC-E4-1 capture key). Inherited
        // CLAUDE_CODE_SESSION_ID must be dropped so the child does not cross-write
        // the parent's transcript.
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("CLAUDE_CODE_SESSION_ID", "inherited-parent-id");
        let cfg = WorkerConfig::new(
            "wk-claude",
            PathBuf::from("/tmp/abi-test-home-3"),
            PathBuf::from("/tmp"),
            vec![
                "claude".to_string(),
                "--session-id".to_string(),
                "u1".to_string(),
            ],
        );
        let cmd = build_child_command(&cfg);
        // Capture before cleanup so a failing assert can't leak the var to the
        // next test that takes the lock.
        let force = cmd
            .get_env("CLAUDE_CODE_FORCE_SESSION_PERSISTENCE")
            .map(|s| s.to_owned());
        let inherited_sid = cmd.get_env("CLAUDE_CODE_SESSION_ID").map(|s| s.to_owned());
        std::env::remove_var("CLAUDE_CODE_SESSION_ID");
        drop(_g);
        assert_eq!(
            force.as_deref(),
            Some(OsStr::new("1")),
            "interactive claude must force session persistence so the jsonl exists"
        );
        assert_eq!(
            inherited_sid, None,
            "the inherited parent session id must be dropped for the claude child"
        );
    }

    #[test]
    fn build_child_command_skips_recipe_for_codex() {
        // The recipe is claude-only: a codex pane's env stays byte-unchanged.
        let cfg = WorkerConfig::new(
            "wk-codex",
            PathBuf::from("/tmp/abi-test-home-4"),
            PathBuf::from("/tmp"),
            vec!["codex".to_string()],
        );
        let cmd = build_child_command(&cfg);
        assert_eq!(
            cmd.get_env("CLAUDE_CODE_FORCE_SESSION_PERSISTENCE"),
            None,
            "non-claude panes must not get the claude persistence recipe"
        );
    }

    // Unix-socket paths are bounded by `sun_path` (~104 bytes on macOS), so a
    // socket-binding test must root under a SHORT path. `/tmp/<short>` keeps
    // `<root>/<short_id>/worker.sock` comfortably under the limit; the verbose
    // `std::env::temp_dir()` path on macOS (`/var/folders/...`) does not. The
    // pid + atomic counter make the path collision-proof across parallel tests
    // (a plain timestamp can collide when tests start in the same millisecond).
    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        PathBuf::from(format!("/tmp/abiw{tag}{}_{}", std::process::id(), n))
    }

    /// Spawn the worker on a background current-thread runtime and return its
    /// home + short_id once the socket is up. Drives a real `cat` child (stays
    /// alive on its stdin).
    async fn start_worker(home: &PathBuf, short_id: &str) {
        let cfg = WorkerConfig::new(
            short_id,
            home.clone(),
            std::env::temp_dir(),
            vec!["cat".to_string()],
        );
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                if let Err(e) = run(cfg).await {
                    eprintln!("WORKER RUN ERROR: {e}");
                }
            });
        });
        // Wait for the socket to appear.
        let sock = AgentsHome::at(home).worker_sock(short_id);
        let start = Instant::now();
        while !sock.exists() && start.elapsed() < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(sock.exists(), "worker socket never appeared");
    }

    /// Connect to a just-bound socket, retrying briefly. A freshly-bound
    /// listener under heavy parallel test load can momentarily refuse before its
    /// accept loop is scheduled; the production client retries the same way
    /// (`client::ensure_daemon`).
    async fn connect_retry(sock: &std::path::Path) -> UnixStream {
        let start = Instant::now();
        loop {
            match UnixStream::connect(sock).await {
                Ok(c) => return c,
                Err(_) if start.elapsed() < Duration::from_secs(3) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => panic!("connect to {} failed: {e}", sock.display()),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn worker_serves_ping_write_snapshot() {
        let home = tmp_home("rpc");
        start_worker(&home, "wkA").await;
        let sock = AgentsHome::at(&home).worker_sock("wkA");

        let mut conn = connect_retry(&sock).await;
        write_request(&mut conn, &Request::new(1, "worker.ping", json!({})))
            .await
            .unwrap();
        let resp = read_response(&mut conn).await.unwrap();
        assert!(!resp.is_err());
        assert_eq!(resp.result().unwrap()["pong"], true);

        // Write to cat's stdin; it echoes back onto the PTY.
        write_request(
            &mut conn,
            &Request::new(2, "worker.write", json!({"data": "hello-pty\n"})),
        )
        .await
        .unwrap();
        let _ = read_response(&mut conn).await.unwrap();

        // Snapshot should eventually contain the echoed text.
        let mut seen = false;
        for i in 0..50 {
            write_request(
                &mut conn,
                &Request::new(100 + i, "worker.snapshot", json!({})),
            )
            .await
            .unwrap();
            let r = read_response(&mut conn).await.unwrap();
            if r.result().unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("hello-pty")
            {
                seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(seen, "echoed PTY output never appeared in snapshot");

        // Status reports a live child.
        write_request(&mut conn, &Request::new(3, "worker.status", json!({})))
            .await
            .unwrap();
        let r = read_response(&mut conn).await.unwrap();
        assert_eq!(r.result().unwrap()["child_alive"], true);

        // Shutdown ends the worker.
        write_request(&mut conn, &Request::new(4, "worker.shutdown", json!({})))
            .await
            .unwrap();
        let r = read_response(&mut conn).await.unwrap();
        assert_eq!(r.result().unwrap()["shutdown"], true);

        std::fs::remove_dir_all(&home).ok();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn worker_publishes_live_state() {
        let home = tmp_home("state");
        start_worker(&home, "wkB").await;
        let state_path = AgentsHome::at(&home).state_json("wkB");
        let st = state::load_state(&state_path).unwrap().unwrap();
        assert_eq!(st.status, AgentStatus::Live);
        assert!(st.pty.unwrap().active);

        // shut it down
        let sock = AgentsHome::at(&home).worker_sock("wkB");
        let mut conn = connect_retry(&sock).await;
        write_request(&mut conn, &Request::new(1, "worker.shutdown", json!({})))
            .await
            .unwrap();
        let _ = read_response(&mut conn).await;
        std::fs::remove_dir_all(&home).ok();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn worker_read_since_streams_incrementally_and_binary_write_roundtrips() {
        let home = tmp_home("drive");
        start_worker(&home, "wkD").await;
        let sock = AgentsHome::at(&home).worker_sock("wkD");
        let mut conn = connect_retry(&sock).await;

        // Drive-path input: raw bytes via base64 (here a control byte + text).
        let raw = b"\x01drive-bytes\n";
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        write_request(
            &mut conn,
            &Request::new(1, "worker.write", json!({ "bytes_b64": b64 })),
        )
        .await
        .unwrap();
        let r = read_response(&mut conn).await.unwrap();
        assert!(!r.is_err());
        assert_eq!(r.result().unwrap()["written"], raw.len());

        // Incremental read: poll read_since from cursor 0 until cat echoes the
        // text back, advancing the cursor each poll. The accumulated decoded
        // output must contain the echoed payload.
        let mut cursor = 0u64;
        let mut acc: Vec<u8> = Vec::new();
        let mut seen = false;
        for i in 0..50 {
            write_request(
                &mut conn,
                &Request::new(100 + i, "worker.read_since", json!({ "cursor": cursor })),
            )
            .await
            .unwrap();
            let resp = read_response(&mut conn).await.unwrap();
            let res = resp.result().unwrap();
            cursor = res["next_offset"].as_u64().unwrap();
            let chunk = base64::engine::general_purpose::STANDARD
                .decode(res["bytes_b64"].as_str().unwrap())
                .unwrap();
            acc.extend_from_slice(&chunk);
            if acc
                .windows(b"drive-bytes".len())
                .any(|w| w == b"drive-bytes")
            {
                seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(seen, "echoed drive bytes never streamed via read_since");

        // Drain any trailing echo bytes before asserting an empty tail. The PTY
        // child (`cat`) can emit trailing bytes (e.g. a `\r\n` line ending) AFTER
        // the loop above breaks on seeing the payload but BEFORE the tail re-read
        // below, so a single re-read races the child's output (the intermittent
        // "left 2 right 0" CI failure). Poll read_since until a read returns zero
        // new bytes, advancing the cursor past the trailing echo, so the tail
        // assertion is deterministic. (Fixes flaky cv-ea4e1f0c.)
        for _ in 0..50 {
            write_request(
                &mut conn,
                &Request::new(150, "worker.read_since", json!({ "cursor": cursor })),
            )
            .await
            .unwrap();
            let resp = read_response(&mut conn).await.unwrap();
            let res = resp.result().unwrap();
            let chunk_len = base64::engine::general_purpose::STANDARD
                .decode(res["bytes_b64"].as_str().unwrap())
                .unwrap()
                .len();
            cursor = res["next_offset"].as_u64().unwrap();
            if chunk_len == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Cursor is now at the tail: a re-read returns no new bytes.
        write_request(
            &mut conn,
            &Request::new(200, "worker.read_since", json!({ "cursor": cursor })),
        )
        .await
        .unwrap();
        let resp = read_response(&mut conn).await.unwrap();
        let res = resp.result().unwrap();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(res["bytes_b64"].as_str().unwrap())
                .unwrap()
                .len(),
            0,
            "re-read at the tail must be empty"
        );
        assert_eq!(res["child_alive"], true);

        write_request(&mut conn, &Request::new(3, "worker.shutdown", json!({})))
            .await
            .unwrap();
        let _ = read_response(&mut conn).await;
        std::fs::remove_dir_all(&home).ok();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_method_is_structured_error() {
        let home = tmp_home("unknown");
        start_worker(&home, "wkC").await;
        let sock = AgentsHome::at(&home).worker_sock("wkC");
        let mut conn = connect_retry(&sock).await;
        write_request(&mut conn, &Request::new(1, "worker.bogus", json!({})))
            .await
            .unwrap();
        let r = read_response(&mut conn).await.unwrap();
        assert!(r.is_err());
        assert_eq!(r.error().unwrap().code, ErrorCode::UnknownMethod);

        write_request(&mut conn, &Request::new(2, "worker.shutdown", json!({})))
            .await
            .unwrap();
        let _ = read_response(&mut conn).await;
        std::fs::remove_dir_all(&home).ok();
    }
}
