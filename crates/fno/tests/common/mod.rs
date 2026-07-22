//! Shared e2e harness, two seams:
//!
//! - [`ClientHarness`]: the real `fno` client on a real portable-pty, with a
//!   human-eye view of its output through our own VT emulator
//!   (`fno::vt::Pane`). Used by `client_e2e.rs` and `persistence.rs`.
//! - [`FakeClient`]: a raw `UnixStream` speaking the wire protocol against a
//!   real headless server - the layout e2e seam (task 2.6). It tracks the
//!   latest `Layout`, per-pane frames + counts (the AC5-EDGE
//!   no-frames-for-inactive assertion), `ModeSync` bytes, and `Notice`s.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use fno::proto::{
    write_msg_sync, AgentRow, BlockDir, ClientMsg, Command, Frame, MouseEvent, ServerMsg,
    SquadMeta, BUILD_VERSION, MAX_MSG_BYTES, PROTO_VERSION,
};
use fno::tree::Rect;
use fno::vt::{frame_text, Pane};

/// One outcome of a read off the fake client's socket. Idle (nothing yet) and
/// Closed (peer gone) must stay distinct: the wait loops retry on Idle.
#[allow(dead_code)]
enum Wire {
    Msg(ServerMsg),
    Idle,
    Closed,
}

pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("fno-e2e-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    /// The session socket the client will use under `FNO_MUX_DIR`.
    /// (Each integration target compiles this module separately, so helpers
    /// only one target uses look dead in the others.)
    #[allow(dead_code)]
    pub fn main_sock(&self) -> PathBuf {
        self.0.join("main.sock")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The `fno` client running on a real PTY, plus a human-eye view of it.
pub struct ClientHarness {
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
    consumed: usize,
    pub pane: Pane,
    /// The scratch dir, for post-mortem server-log dumps (x-0296): the
    /// autospawned server's stderr lands in `<session>.log` here, and the
    /// dir is gone before anyone can read it once the test unwinds.
    scratch_dir: PathBuf,
    // Keep the master alive for the harness lifetime.
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl ClientHarness {
    pub fn spawn(scratch: &Scratch) -> Self {
        Self::spawn_full(scratch, &[], &[])
    }

    /// Like [`ClientHarness::spawn`] with extra environment on the client
    /// process (the nested-guard cases need `FNO_SESSION` preset).
    pub fn spawn_with(scratch: &Scratch, envs: &[(&str, &str)]) -> Self {
        Self::spawn_full(scratch, envs, &[])
    }

    /// Like [`ClientHarness::spawn`] but attaching an explicit `--session`.
    /// Bare `fno` runs the pre-attach session picker (US5); a fake server
    /// that `accept()`s only once is consumed by the picker's live/stale
    /// probe before the real attach, so a test wanting a DIRECT attach names
    /// the session outright and bypasses the picker (AC5-FR).
    #[allow(dead_code)]
    pub fn spawn_session(scratch: &Scratch, session: &str) -> Self {
        Self::spawn_full(scratch, &[], &["--session", session])
    }

    fn spawn_full(scratch: &Scratch, envs: &[(&str, &str)], args: &[&str]) -> Self {
        // 60 columns: below the sideline's auto-hide threshold (panel 28 +
        // min content 40), so the panel stays hidden and Phase-1-era screen
        // assertions see bare content lines under the 1-row tab bar. The
        // sideline-visible chrome has its own compose unit tests + the
        // layout e2e suite; here it would only salt every line with the
        // divider column.
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 60,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_fno"));
        for a in args {
            cmd.arg(a);
        }
        cmd.env("FNO_MUX_DIR", &scratch.0);
        cmd.env("SHELL", "/bin/sh");
        cmd.env("TERM", "xterm-256color");
        // A bare, predictable prompt so screen assertions are stable.
        cmd.env("PS1", "$ ");
        // Reap any server this client autospawns (x-4e30): spawn_server
        // inherits the env (no env_clear), so the marker reaches the
        // setsid'd, harness-untracked server and it self-exits within grace.
        cmd.env("FNO_E2E", "1");
        // Same hermetic isolation as spawn_server: the autospawned server reads
        // the agent registry + claude-daemon roster from empty scratch subdirs,
        // so it neither sees the developer's live agents nor writes events to the
        // real ~/.fno. Overridable via `envs` below.
        cmd.env("FNO_AGENTS_HOME", scratch.0.join("iso-agents"));
        cmd.env("FNO_CLAUDE_DAEMON_DIR", scratch.0.join("iso-daemon"));
        // See spawn_server: neutralize isolated-account roster discovery too.
        cmd.env(
            "FNO_GLOBAL_SETTINGS_PATH",
            scratch.0.join("iso-cfg").join("settings.json"),
        );
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let child = pty.slave.spawn_command(cmd).unwrap();
        drop(pty.slave);

        let writer = pty.master.take_writer().unwrap();
        let mut reader = pty.master.try_clone_reader().unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = output.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        ClientHarness {
            child,
            writer,
            output,
            consumed: 0,
            pane: Pane::new(24, 60),
            scratch_dir: scratch.0.clone(),
            _master: pty.master,
        }
    }

    pub fn type_bytes(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    /// Feed anything new from the client into the emulator, return the screen.
    pub fn screen(&mut self) -> String {
        let out = self.output.lock().unwrap();
        if out.len() > self.consumed {
            self.pane.feed(&out[self.consumed..]);
            self.consumed = out.len();
        }
        drop(out);
        self.pane.text()
    }

    /// Everything the client ever wrote, raw (pre-TUI prints included).
    #[allow(dead_code)]
    pub fn raw_output(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).to_string()
    }

    pub fn wait_screen(&mut self, secs: u64, pred: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let screen = self.screen();
            if pred(&screen) {
                return screen;
            }
            if Instant::now() >= deadline {
                panic!(
                    "screen never matched within {secs}s; last screen:\n{screen}\n{}",
                    self.diagnostics()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Everything a CI-only timeout needs to be diagnosed from the job log
    /// alone (x-0296): the client's raw byte stream (pre/post-TUI eprintln
    /// lines land here - "previous session ended", "session ended (server
    /// closed)", spawn errors) and every server log in the scratch dir (the
    /// setsid'd server's stderr, destroyed with the scratch before anyone
    /// can read it otherwise). Distinguishes a dead/respawned/wedged server
    /// from lost frame delivery, which the settled screen alone cannot.
    pub fn diagnostics(&self) -> String {
        // ESC made visible so the panic message stays one readable block.
        fn tail(s: &str, max: usize) -> &str {
            let mut start = s.len().saturating_sub(max);
            while !s.is_char_boundary(start) {
                start += 1;
            }
            &s[start..]
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let raw = self.raw_output().replace('\x1b', "\\e");
        let mut out = format!(
            "--- diagnostics at unix_ms {now_ms}, client pid {:?} ---\n--- client raw output ({} bytes, tail) ---\n{}\n",
            self.child.process_id(),
            raw.len(),
            tail(&raw, 3000)
        );
        if let Ok(rd) = std::fs::read_dir(&self.scratch_dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "sock") {
                    // Is anything still listening? Refused/absent = the server
                    // process is gone; connected = at least the OS-level
                    // listener survives (a wedged runtime still accepts via
                    // the backlog, so this cannot prove liveness - only death).
                    let probe = std::os::unix::net::UnixStream::connect(&p);
                    out.push_str(&format!(
                        "--- socket {}: {} ---\n",
                        p.display(),
                        match &probe {
                            Ok(_) => "accepting connections".to_string(),
                            Err(e) => format!("connect failed: {e}"),
                        }
                    ));
                }
                if p.extension().is_some_and(|x| x == "log") {
                    let body = std::fs::read_to_string(&p).unwrap_or_default();
                    out.push_str(&format!(
                        "--- server log {} ({} bytes, tail) ---\n{}\n",
                        p.display(),
                        body.len(),
                        tail(&body, 3000)
                    ));
                }
            }
        }
        out
    }

    /// Wait until the shell sits at a fresh prompt: a fresh prompt line ends
    /// with the pinned `PS1` (`$ `). Required after an interrupt (^C) before
    /// typing again - until the shell regains the foreground, the tty line
    /// discipline can flush/drop bytes typed at the dying process (codex P1:
    /// consistently reproducible on Linux PTYs).
    ///
    /// Scans the last two non-empty lines rather than only the last: the
    /// always-on status row (US4) is client-local chrome that renders as the
    /// final non-empty line and never ends with `$`, so keying on the very
    /// last line alone would never see the prompt sitting just above it.
    pub fn wait_prompt(&mut self, secs: u64) -> String {
        self.wait_screen(secs, |s| {
            s.lines()
                .rev()
                .filter(|l| !l.trim().is_empty())
                .take(2)
                .any(|l| l.trim_end().ends_with('$'))
        })
    }

    pub fn wait_exit(&mut self, secs: u64) -> portable_pty::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() >= deadline {
                panic!("client did not exit within {secs}s");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for ClientHarness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Headless server + fake wire client (the layout e2e seam)
// ---------------------------------------------------------------------------

/// A `fno --server` child, always killed on test exit.
pub struct ServerProc(pub std::process::Child);

impl Drop for ServerProc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn the real server binary headless on `sock`, with `envs` overriding
/// the inherited environment (SHELL, PATH for the git-stub cases, ...).
#[allow(dead_code)]
pub fn spawn_server(sock: &Path, envs: &[(&str, &str)]) -> ServerProc {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_fno"));
    cmd.args(["--server"])
        .arg(sock)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Reap the server if this test process dies without running Drop (SIGKILL,
    // panic=abort, cargo-test timeout) — x-4e30. A test that needs a specific
    // grace (or none) overrides via `envs`, which is applied after.
    cmd.env("FNO_E2E", "1");
    // Hermetic sideline: point the agent registry and the claude-daemon roster
    // at empty scratch subdirs so the server enumerates only THIS test's panes,
    // never the developer's live agents. Without this the server reads the real
    // ~/.fno/agents/registry.json + roster and injects phantom squads/rows,
    // which shifts squad ids and flakes the layout/multiclient selection tests
    // whenever a real mux daemon is running. It also reads the real
    // ~/.fno/squads.json (the spawned binary is built without cfg(test), so the
    // in-process TEST_PATH seam never reaches it) and inherits live named
    // squads, which breaks squad-count assertions on a machine that has them (a
    // clean CI home hides the leak). A caller that needs a real registry (e.g.
    // agent_edge_e2e) overrides via `envs`, applied after.
    let iso = sock.parent().unwrap_or_else(|| Path::new("."));
    cmd.env("FNO_AGENTS_HOME", iso.join("iso-agents"));
    cmd.env("FNO_CLAUDE_DAEMON_DIR", iso.join("iso-daemon"));
    // Same leak, one reader over: the backlog reader resolves FNO_GRAPH_JSON >
    // $HOME/.fno/graph.json, so without this the server derives Backlog cards AND
    // mission squads from the DEVELOPER'S real graph. An active mission adds a
    // synthetic squad to the sideline and breaks squad-count assertions - the
    // failure follows whatever happens to be in flight on the machine, which is
    // why it reads as a flake, and a clean CI home hides it entirely. The path
    // deliberately does not exist: no graph means an empty lane and no missions.
    cmd.env("FNO_GRAPH_JSON", iso.join("iso-graph.json"));
    // Isolated-account rosters are discovered via the provider config
    // (isolated_account_dirs -> $PWD/.fno/config.toml, else this override's
    // sibling config.toml, else ~/.fno/config.toml). Point the override at an
    // empty scratch dir so a multi-account developer's live alt-account workers
    // don't fold into the sideline either.
    cmd.env(
        "FNO_GLOBAL_SETTINGS_PATH",
        iso.join("iso-cfg").join("settings.json"),
    );
    for (k, v) in envs {
        cmd.env(k, v);
    }
    ServerProc(cmd.spawn().unwrap())
}

#[allow(dead_code)]
pub fn connect_with_retry(sock: &Path) -> std::os::unix::net::UnixStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match std::os::unix::net::UnixStream::connect(sock) {
            Ok(s) => return s,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("server never came up at {}: {e}", sock.display()),
        }
    }
}

/// The last `Layout` a [`FakeClient`] saw.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub struct LayoutSnap {
    pub squads: Vec<SquadMeta>,
    pub active_squad: u64,
    pub panes: Vec<(u64, Rect)>,
    pub focus: u64,
    pub area: (u16, u16),
    pub agents: Vec<AgentRow>,
    pub focus_node: Option<String>,
}

/// One absorbed message kind, in arrival order - the seam for asserting the
/// re-anchor ordering contract (ModeSync -> Layout -> frames, AC2-ERR).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Absorbed {
    ModeSync,
    Layout,
    Frame(u64),
}

/// A raw wire client: sends `ClientMsg`s, absorbs everything the server
/// streams back into inspectable state.
#[allow(dead_code)]
pub struct FakeClient {
    stream: std::os::unix::net::UnixStream,
    pub layout: Option<LayoutSnap>,
    pub frames: HashMap<u64, Frame>,
    /// Frames received per pane since the last [`FakeClient::reset_counts`] -
    /// the wire-silence assertion for inactive tabs (AC5-EDGE).
    pub frame_counts: HashMap<u64, usize>,
    pub modesyncs: Vec<Vec<u8>>,
    pub notices: Vec<String>,
    pub byes: Vec<String>,
    /// Server-extracted copy payloads (v7, US2), newest last.
    pub copies: Vec<String>,
    /// Initiator-only search results (v12, x-e780): `(pane_id, total, current)`,
    /// newest last. A co-viewer never receives these.
    pub search_results: Vec<(u64, u32, u32)>,
    /// Every absorbed message's kind, chronologically.
    pub order: Vec<Absorbed>,
    /// Bytes read off the socket that do not yet form a whole message.
    ///
    /// The stream carries length-prefixed frames and the socket has a short read
    /// timeout so the wait loops can poll. Those two only compose if a partial
    /// read is KEPT: `read_exact` discards what it consumed when it errors, so a
    /// body that straddles the timeout used to be dropped, and the next read took
    /// mid-body bytes for a length prefix - a desync that surfaced as an absurd
    /// "message of N bytes exceeds the cap" panic under load.
    carry: Vec<u8>,
}

#[allow(dead_code)]
impl FakeClient {
    /// Connect + Attach (content-area `rows`x`cols`, squad-keying `cwd`).
    pub fn attach(sock: &Path, rows: u16, cols: u16, cwd: &str) -> Self {
        let stream = connect_with_retry(sock);
        let mut w = stream.try_clone().unwrap();
        write_msg_sync(
            &mut w,
            &ClientMsg::Attach {
                proto: PROTO_VERSION,
                build: BUILD_VERSION.to_string(),
                rows,
                cols,
                cwd: cwd.to_string(),
            },
        )
        .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        FakeClient {
            stream,
            layout: None,
            frames: HashMap::new(),
            frame_counts: HashMap::new(),
            modesyncs: Vec::new(),
            notices: Vec::new(),
            byes: Vec::new(),
            copies: Vec::new(),
            search_results: Vec::new(),
            order: Vec::new(),
            carry: Vec::new(),
        }
    }

    pub fn input(&mut self, bytes: &[u8]) {
        let mut w = self.stream.try_clone().unwrap();
        write_msg_sync(&mut w, &ClientMsg::Input(bytes.to_vec())).unwrap();
    }

    pub fn cmd(&mut self, cmd: Command) {
        let mut w = self.stream.try_clone().unwrap();
        write_msg_sync(&mut w, &ClientMsg::Command(cmd)).unwrap();
    }

    /// Forward a pane-local mouse event (v7): what the real client sends
    /// after hit-testing an outer-terminal event into a pane rect.
    pub fn mouse(&mut self, pane: u64, event: MouseEvent) {
        let mut w = self.stream.try_clone().unwrap();
        write_msg_sync(&mut w, &ClientMsg::Mouse { pane, event }).unwrap();
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let mut w = self.stream.try_clone().unwrap();
        write_msg_sync(&mut w, &ClientMsg::Resize { rows, cols }).unwrap();
    }

    pub fn detach(&mut self) {
        let mut w = self.stream.try_clone().unwrap();
        write_msg_sync(&mut w, &ClientMsg::Detach).unwrap();
    }

    /// (v12, x-e780) Open/step/clear an in-scrollback search on `pane`.
    pub fn search_open(&mut self, pane: u64, query: &str) {
        let mut w = self.stream.try_clone().unwrap();
        write_msg_sync(
            &mut w,
            &ClientMsg::SearchOpen {
                pane,
                query: query.to_string(),
            },
        )
        .unwrap();
    }

    pub fn search_step(&mut self, pane: u64, dir: BlockDir) {
        let mut w = self.stream.try_clone().unwrap();
        write_msg_sync(&mut w, &ClientMsg::SearchStep { pane, dir }).unwrap();
    }

    pub fn search_clear(&mut self, pane: u64) {
        let mut w = self.stream.try_clone().unwrap();
        write_msg_sync(&mut w, &ClientMsg::SearchClear { pane }).unwrap();
    }

    pub fn reset_counts(&mut self) {
        self.frame_counts.clear();
    }

    fn absorb(&mut self, msg: ServerMsg) {
        match &msg {
            ServerMsg::Frame { pane_id, .. } => self.order.push(Absorbed::Frame(*pane_id)),
            ServerMsg::Layout { .. } => self.order.push(Absorbed::Layout),
            ServerMsg::ModeSync { .. } => self.order.push(Absorbed::ModeSync),
            _ => {}
        }
        match msg {
            ServerMsg::Frame { pane_id, frame } => {
                assert!(frame.geometry_ok(), "server sent a malformed frame");
                *self.frame_counts.entry(pane_id).or_insert(0) += 1;
                self.frames.insert(pane_id, frame);
            }
            ServerMsg::Layout {
                squads,
                active_squad,
                panes,
                focus,
                area,
                agents,
                focus_node,
                .. // backlog (x-6f77): the e2e harness asserts nothing on it
            } => {
                self.layout = Some(LayoutSnap {
                    squads,
                    active_squad,
                    panes,
                    focus,
                    area,
                    agents,
                    focus_node,
                });
            }
            ServerMsg::ModeSync { bytes } => self.modesyncs.push(bytes),
            ServerMsg::Notice { text } => self.notices.push(text),
            ServerMsg::Bye { reason } => self.byes.push(reason),
            ServerMsg::Copy { text } => self.copies.push(text),
            ServerMsg::SearchResult {
                pane_id,
                total,
                current,
            } => self.search_results.push((pane_id, total, current)),
            // Answers a pre-Attach Query only; stray on an attached client.
            ServerMsg::Info { .. } => {}
            // v4 control-verb replies belong to one-shot `fno mux pane`
            // connections, never this attached client - ignore.
            ServerMsg::PaneList { .. }
            | ServerMsg::PaneText { .. }
            | ServerMsg::PaneSpawned { .. }
            | ServerMsg::Ok
            | ServerMsg::WaitDone { .. }
            | ServerMsg::Err { .. }
            // (x-d865) v41 layout script-verb replies: one-shot control
            // connections only, never this attached client - ignore.
            | ServerMsg::TabList { .. }
            | ServerMsg::LayoutTree { .. }
            | ServerMsg::PaneLocation { .. }
            | ServerMsg::TabSpawned { .. }
            | ServerMsg::LayoutApplied { .. } => {}
            // (x-c376) Peek transcript body: a client-interactive reply covered
            // by client unit tests, not the e2e absorber - ignore here.
            ServerMsg::PeekBody { .. } => {}
        }
    }

    /// Read the next whole message.
    ///
    /// Resumable by construction: bytes land in `carry` via plain `read` (which
    /// never discards a short read) and a message is decoded only once its whole
    /// body is buffered. A timeout mid-body is therefore just [`Wire::Idle`] -
    /// the stream can never lose framing, however the reads happen to split.
    ///
    /// Idle and closed are distinct on purpose: the callers retry on Idle, so
    /// collapsing the two would spin a closed socket at full tilt until the
    /// outer deadline and report a timeout where the truth was a disconnect.
    fn next_msg(&mut self) -> Wire {
        loop {
            if let Some(msg) = self.take_framed() {
                return Wire::Msg(msg);
            }
            let mut buf = [0u8; 64 * 1024];
            match self.stream.read(&mut buf) {
                Ok(0) => return Wire::Closed,
                Ok(n) => self.carry.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                    return Wire::Idle
                }
                Err(e) => panic!("fake client read failed: {e}"),
            }
        }
    }

    /// Pop one complete length-prefixed message out of `carry`, if one is there.
    fn take_framed(&mut self) -> Option<ServerMsg> {
        let len = u32::from_be_bytes(self.carry.get(..4)?.try_into().ok()?) as usize;
        assert!(
            len <= MAX_MSG_BYTES as usize,
            "framing desync: length prefix claims {len} bytes"
        );
        if self.carry.len() < 4 + len {
            return None; // body still arriving
        }
        let body: Vec<u8> = self.carry.drain(..4 + len).skip(4).collect();
        Some(serde_json::from_slice(&body).expect("server sent undecodable message"))
    }

    /// Absorb whatever arrives for `dur` (used for wire-SILENCE windows).
    pub fn pump(&mut self, dur: Duration) {
        let deadline = Instant::now() + dur;
        while Instant::now() < deadline {
            match self.next_msg() {
                Wire::Msg(m) => self.absorb(m),
                Wire::Idle => {}
                Wire::Closed => return,
            }
        }
    }

    /// Absorb until `f` yields, or panic with the current state at `secs`.
    pub fn wait<T>(&mut self, secs: u64, what: &str, f: impl Fn(&FakeClient) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(v) = f(self) {
                return v;
            }
            if Instant::now() >= deadline {
                panic!(
                    "never saw {what} within {secs}s; layout: {:?}; notices: {:?}; pane texts: {:?}",
                    self.layout,
                    self.notices,
                    self.frames
                        .iter()
                        .map(|(id, f)| (*id, frame_text(f)))
                        .collect::<Vec<_>>(),
                );
            }
            match self.next_msg() {
                Wire::Msg(m) => self.absorb(m),
                Wire::Idle => {}
                Wire::Closed => panic!("server closed the stream while waiting for {what}"),
            }
        }
    }

    pub fn wait_layout(
        &mut self,
        secs: u64,
        what: &str,
        pred: impl Fn(&LayoutSnap) -> bool,
    ) -> LayoutSnap {
        self.wait(secs, what, |c| c.layout.clone().filter(|l| pred(l)))
    }

    pub fn pane_text(&self, pid: u64) -> String {
        self.frames.get(&pid).map(frame_text).unwrap_or_default()
    }

    pub fn wait_pane_text(&mut self, secs: u64, pid: u64, pred: impl Fn(&str) -> bool) -> String {
        self.wait(secs, &format!("pane {pid} text"), |c| {
            Some(c.pane_text(pid)).filter(|t| pred(t))
        })
    }

    /// Gate the FIRST input to a pane on the shell being ready. Typing before
    /// the shell reads gets the bytes echoed by the tty line discipline but
    /// never executed (the submitting CR is lost during startup), so a test
    /// that types straight after attach silently loses its command - dash (the
    /// Linux `/bin/sh`) exposes this deterministically, bash-as-sh's slower
    /// startup happens to be racier. Nudge with a bare CR until the prompt
    /// renders: a ready shell re-emits `$ ` on each Enter (also recovering a
    /// startup frame missed under parallel-test load), and a not-yet-ready
    /// shell drops the CR harmlessly so the next nudge retries. Bounded so a
    /// genuinely dead pane still panics with the live state. The pinned prompt
    /// ends with `$` (`sh-3.2$ ` on macOS bash-as-sh, `$ ` on dash).
    ///
    /// Drives the *focused* pane: the CR nudge is routed server-side to the
    /// focused pane, so `pane` must be the focused one (it always is at the
    /// call sites - the first pane right after attach). Asserted to fail loud
    /// rather than silently nudge the wrong pane if that ever changes.
    pub fn wait_prompt(&mut self, pane: u64) {
        assert_eq!(self.focus(), pane, "wait_prompt drives the focused pane");
        for _ in 0..28 {
            if self.pane_text(pane).trim_end().ends_with('$') {
                return;
            }
            self.input(b"\r");
            self.pump(Duration::from_millis(500));
        }
        self.wait_pane_text(15, pane, |t| t.trim_end().ends_with('$'));
    }

    /// The current focused pane per the last Layout.
    pub fn focus(&self) -> u64 {
        self.layout.as_ref().expect("no Layout yet").focus
    }
}
