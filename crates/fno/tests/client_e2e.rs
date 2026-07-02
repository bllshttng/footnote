//! Client e2e tests (task 1.3): the real `fno` client runs under a
//! portable-pty from this harness - a true TTY, so bare `fno` role-selects to
//! the client, spawns its server, attaches, and draws. The harness plays the
//! human: it types into the PTY master and reads the client's rendered output
//! through our own VT emulator (`fno::vt::Pane`), i.e. it asserts on exactly
//! the screen a person would see.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use fno::vt::Pane;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("fno-e2e-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The `fno` client running on a real PTY, plus a human-eye view of it.
struct ClientHarness {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
    consumed: usize,
    pane: Pane,
    // Keep the master alive for the harness lifetime.
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl ClientHarness {
    fn spawn(scratch: &Scratch) -> Self {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_fno"));
        cmd.env("FNO_MUX_DIR", &scratch.0);
        cmd.env("SHELL", "/bin/sh");
        cmd.env("TERM", "xterm-256color");
        // A bare, predictable prompt so screen assertions are stable.
        cmd.env("PS1", "$ ");
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
            pane: Pane::new(24, 80),
            _master: pty.master,
        }
    }

    fn type_bytes(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    /// Feed anything new from the client into the emulator, return the screen.
    fn screen(&mut self) -> String {
        let out = self.output.lock().unwrap();
        if out.len() > self.consumed {
            self.pane.feed(&out[self.consumed..]);
            self.consumed = out.len();
        }
        drop(out);
        self.pane.text()
    }

    fn wait_screen(&mut self, secs: u64, pred: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let screen = self.screen();
            if pred(&screen) {
                return screen;
            }
            if Instant::now() >= deadline {
                panic!("screen never matched within {secs}s; last screen:\n{screen}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_exit(&mut self, secs: u64) -> portable_pty::ExitStatus {
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

#[test]
fn client_e2e_prompt_appears_and_echo_roundtrips() {
    // AC1-HP + AC2-HP: bare `fno` on a TTY comes up with a shell, and typed
    // input round-trips to rendered output. (The 500ms latency target is not
    // asserted - CI wall-clock is not a fairness court; presence is.)
    let scratch = Scratch::new("echo");
    let mut h = ClientHarness::spawn(&scratch);
    // A prompt renders.
    h.wait_screen(15, |s| !s.trim().is_empty());
    h.type_bytes(b"echo he\"ll\"o\r");
    // Only the OUTPUT line is bare "hello" (the typed line has quotes).
    h.wait_screen(15, |s| s.lines().any(|l| l.trim() == "hello"));
    // AC1-UI: the cursor is visible and sits on the last written row (the
    // fresh prompt), where the shell put it.
    let frame = h.pane.frame();
    assert!(frame.cursor_visible, "cursor must be visible at the prompt");
    let text = h.screen();
    let last_row = text.lines().count().saturating_sub(1);
    assert_eq!(
        frame.cursor_row as usize, last_row,
        "cursor should sit on the prompt row; screen:\n{text}"
    );
}

#[test]
fn client_e2e_utf8_and_control_keys_pass_through() {
    // AC2-UI: UTF-8 renders; Ctrl-C interrupts a foreground command. Both are
    // raw-byte passthrough - nothing re-encodes the input.
    let scratch = Scratch::new("bytes");
    let mut h = ClientHarness::spawn(&scratch);
    h.wait_screen(15, |s| !s.trim().is_empty());
    h.type_bytes("echo caf\u{00e9}\r".as_bytes());
    h.wait_screen(15, |s| s.lines().any(|l| l.trim() == "caf\u{00e9}"));
    // Ctrl-C a sleep; the shell survives and answers again.
    h.type_bytes(b"sleep 100\r");
    std::thread::sleep(Duration::from_millis(300));
    h.type_bytes(&[0x03]); // ^C
    h.type_bytes(b"echo interrupted\r");
    h.wait_screen(15, |s| s.lines().any(|l| l.trim() == "interrupted"));
}

#[test]
fn client_e2e_detach_exits_client_and_leaves_server_running() {
    // The Ctrl-\ detach: client exits 0, and the session (server + shell)
    // stays alive - proven by a fresh client reattaching and seeing state
    // from before the detach. (Full persistence torture is task 1.4.)
    let scratch = Scratch::new("detach");
    let mut h = ClientHarness::spawn(&scratch);
    h.wait_screen(15, |s| !s.trim().is_empty());
    h.type_bytes(b"BEFORE_DETACH=yes\r");
    std::thread::sleep(Duration::from_millis(300));
    h.type_bytes(&[0x1C]); // Ctrl-\ -> detach
    let status = h.wait_exit(10);
    assert!(status.success(), "detach must exit 0, got {status:?}");
    drop(h);

    // Reattach with a new client on the same session.
    let mut h2 = ClientHarness::spawn(&scratch);
    h2.wait_screen(15, |s| !s.trim().is_empty());
    h2.type_bytes(b"echo var=$BEFORE_DETACH\r");
    h2.wait_screen(15, |s| s.lines().any(|l| l.trim() == "var=yes"));
}
