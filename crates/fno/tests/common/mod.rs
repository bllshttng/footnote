//! Shared e2e harness: the real `fno` client on a real portable-pty, with a
//! human-eye view of its output through our own VT emulator (`fno::vt::Pane`).
//! Used by `client_e2e.rs` (task 1.3) and `persistence.rs` (task 1.4).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use fno::vt::Pane;

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
    // Keep the master alive for the harness lifetime.
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl ClientHarness {
    pub fn spawn(scratch: &Scratch) -> Self {
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
                panic!("screen never matched within {secs}s; last screen:\n{screen}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
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
