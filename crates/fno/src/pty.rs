//! PTY spawn + reader thread for the mux server.
//!
//! Seeded from `crates/fno-agents/src/pty.rs`, adapted for the mux: there is
//! no output ring here - the VT grid (`vt.rs`) is the retention - so the
//! reader thread forwards raw chunks straight into a bounded tokio channel
//! the server core loop consumes. PTY master reads are blocking, so they live
//! on a dedicated thread and tokio stays at the edges (the herdr model). When
//! the channel is full the reader blocks, which backpressures the child via
//! the kernel PTY buffer - bounded memory, never an unbounded server-side
//! queue (AC2-EDGE).

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("failed to open pty: {0}")]
    OpenPty(String),
    #[error("no spawnable shell: {0}")]
    Spawn(String),
    #[error("failed to obtain pty writer: {0}")]
    Writer(String),
    #[error("failed to obtain pty reader: {0}")]
    Reader(String),
    #[error("pty write failed: {0}")]
    Write(std::io::Error),
    #[error("pty resize failed: {0}")]
    Resize(String),
}

/// Resolve the shell candidates to try, in order: `$SHELL` (when set and
/// non-empty) then `/bin/sh` (AC1-ERR fallback). Pure so it is unit-testable.
pub fn shell_candidates(env_shell: Option<&str>) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(s) = env_shell {
        let s = s.trim();
        if !s.is_empty() {
            v.push(s.to_string());
        }
    }
    if !v.iter().any(|s| s == "/bin/sh") {
        v.push("/bin/sh".to_string());
    }
    v
}

/// A live PTY-managed shell. The server owns this for the pane's lifetime -
/// client attach/detach never touches it (AC4-HP).
pub struct PtyShell {
    // Held for the fd + resize; boxed trait object as returned by portable-pty.
    master: Box<dyn MasterPty + Send>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
}

impl PtyShell {
    /// Spawn the first spawnable candidate from `candidates` on a fresh
    /// `rows`x`cols` PTY. Output chunks flow into `tx`; the channel closing
    /// (sender dropped) is the child-exited signal to the consumer.
    pub fn spawn(
        candidates: &[String],
        rows: u16,
        cols: u16,
        tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    ) -> Result<PtyShell, PtyError> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PtyError::OpenPty(e.to_string()))?;

        let mut errors = Vec::new();
        let mut child = None;
        for cand in candidates {
            let mut cmd = CommandBuilder::new(cand);
            // A login-ish TERM so shells and full-screen programs behave.
            cmd.env("TERM", "xterm-256color");
            match pair.slave.spawn_command(cmd) {
                Ok(c) => {
                    child = Some(c);
                    break;
                }
                Err(e) => errors.push(format!("{cand}: {e}")),
            }
        }
        let child = child.ok_or_else(|| PtyError::Spawn(errors.join("; ")))?;

        // Standard pattern: drop the slave so only the child holds it.
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| PtyError::Writer(e.to_string()))?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| PtyError::Reader(e.to_string()))?;

        spawn_reader(reader, tx)?;

        Ok(PtyShell {
            master: pair.master,
            writer: Mutex::new(writer),
            child: Mutex::new(child),
        })
    }

    /// Write keystrokes to the child. The caller decides what a failure means
    /// (the server drops input fail-closed once the child has exited, AC2-ERR).
    pub fn write_input(&self, bytes: &[u8]) -> Result<(), PtyError> {
        let mut w = self
            .writer
            .lock()
            .map_err(|_| PtyError::Write(std::io::Error::other("writer mutex poisoned")))?;
        w.write_all(bytes).map_err(PtyError::Write)?;
        w.flush().map_err(PtyError::Write)?;
        Ok(())
    }

    /// Resize the PTY. Pixel dimensions ride along for programs that read
    /// `ws_xpixel`/`ws_ypixel`; pass 0 when unknown (the Phase-1 protocol does
    /// not carry pixels yet).
    pub fn resize(
        &self,
        rows: u16,
        cols: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> Result<(), PtyError> {
        self.master
            .resize(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width,
                pixel_height,
            })
            .map_err(|e| PtyError::Resize(e.to_string()))
    }

    /// True while the child has not exited. Errs toward "alive" on any
    /// uncertainty (same rationale as the fno-agents seed: the consumer uses
    /// this to gate cleanup, and a false "exited" is the dangerous answer).
    pub fn is_child_alive(&self) -> bool {
        let mut child = match self.child.lock() {
            Ok(c) => c,
            Err(_) => return true,
        };
        !matches!(child.try_wait(), Ok(Some(_)))
    }
}

/// The blocking reader thread: PTY master -> bounded channel. Exits on EOF
/// (child gone; macOS reports Ok(0), Linux EIO) or a closed channel, dropping
/// `tx` either way so the consumer sees the stream end.
fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
) -> Result<(), PtyError> {
    std::thread::Builder::new()
        .name("fno-mux-pty-reader".into())
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        // blocking_send backpressures the reader (and thus the
                        // child) when the core loop lags; never unbounded.
                        if tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break; // consumer gone; nothing to drain for
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        // Linux: EIO when the slave side closes = normal exit.
                        if e.raw_os_error() != Some(libc::EIO) {
                            eprintln!("fno mux: pty read faulted: {e}");
                        }
                        break;
                    }
                }
            }
        })
        .map_err(|e| PtyError::Reader(format!("reader thread spawn failed: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_spine_shell_candidates_prefer_env_then_sh() {
        assert_eq!(
            shell_candidates(Some("/bin/zsh")),
            vec!["/bin/zsh", "/bin/sh"]
        );
        assert_eq!(shell_candidates(None), vec!["/bin/sh"]);
        assert_eq!(shell_candidates(Some("")), vec!["/bin/sh"]);
        assert_eq!(shell_candidates(Some("  ")), vec!["/bin/sh"]);
        // No duplicate when $SHELL already is /bin/sh.
        assert_eq!(shell_candidates(Some("/bin/sh")), vec!["/bin/sh"]);
    }

    #[tokio::test]
    async fn server_spine_unspawnable_shell_falls_back() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let candidates = shell_candidates(Some("/nonexistent/definitely-not-a-shell"));
        let shell = PtyShell::spawn(&candidates, 24, 80, tx).expect("fallback must spawn");
        assert!(shell.is_child_alive());
        // Prove the fallback shell is real: round-trip a command.
        shell.write_input(b"echo fallback-ok\r").unwrap();
        let mut seen = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
                Ok(Some(chunk)) => {
                    seen.extend_from_slice(&chunk);
                    if String::from_utf8_lossy(&seen).contains("fallback-ok") {
                        return;
                    }
                }
                Ok(None) => break,
                Err(_) => {}
            }
        }
        panic!(
            "fallback shell never echoed: {:?}",
            String::from_utf8_lossy(&seen)
        );
    }

    #[test]
    fn server_spine_all_candidates_unspawnable_is_a_clear_error() {
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let err = PtyShell::spawn(
            &["/nonexistent/a".into(), "/nonexistent/b".into()],
            24,
            80,
            tx,
        )
        .err()
        .expect("must fail");
        let msg = err.to_string();
        assert!(msg.contains("/nonexistent/a"), "{msg}");
        assert!(msg.contains("/nonexistent/b"), "{msg}");
    }
}
