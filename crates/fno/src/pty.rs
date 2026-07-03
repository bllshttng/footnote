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

use crate::mux_cli::{BASH_SHELL_INIT, ZSH_SHELL_INIT};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
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
/// non-empty) then `/bin/sh` (AC1-ERR fallback). `OsStr` in/out so a non-UTF-8
/// shell path survives without lossy conversion (gemini). Pure so it is
/// unit-testable.
pub fn shell_candidates(env_shell: Option<&OsStr>) -> Vec<OsString> {
    let mut v = Vec::new();
    if let Some(s) = env_shell {
        let is_empty = match s.to_str() {
            Some(utf8) => utf8.trim().is_empty(),
            None => s.is_empty(),
        };
        if !is_empty {
            v.push(s.to_os_string());
        }
    }
    if !v.iter().any(|s| s == OsStr::new("/bin/sh")) {
        v.push(OsString::from("/bin/sh"));
    }
    v
}

/// A live PTY-managed shell. The server owns this for the pane's lifetime -
/// client attach/detach never touches it (AC4-HP).
pub struct PtyShell {
    // Held for the fd + resize; boxed trait object as returned by portable-pty.
    master: Box<dyn MasterPty + Send>,
    // Input goes through a dedicated writer thread (spawn_writer): a PTY
    // master write blocks when the kernel input buffer is full, and doing
    // that on the tokio core loop could deadlock the whole server against a
    // child that is itself blocked on output (gemini high). Bounded queue:
    // when a pathological child stops reading input, keystrokes drop
    // (fail-closed, same policy as input-after-exit) - never unbounded memory.
    input_tx: std::sync::mpsc::SyncSender<Vec<u8>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    // Shell-integration rc temp dir, held purely for cleanup (RAII): dropped
    // when the pane closes / the server exits. `None` for non-shell / off panes.
    _shell_rc: Option<ShellRc>,
}

impl PtyShell {
    /// Spawn the first spawnable candidate from `candidates` on a fresh
    /// `rows`x`cols` PTY, starting the shell in `cwd` when given and still a
    /// directory (a vanished dir degrades to the server's cwd rather than
    /// refusing the pane). Output chunks flow into the SHARED `out_tx` tagged
    /// with `pane_id`; when the child is gone (EOF/EIO on the master) the
    /// reader thread sends `pane_id` on `exit_tx` and ends. An explicit exit
    /// channel replaces Phase 1's channel-close signal: with one shared
    /// output channel for N panes, one reader exiting can no longer close it.
    pub fn spawn(
        candidates: &[OsString],
        rows: u16,
        cols: u16,
        cwd: Option<&std::path::Path>,
        session: &str,
        pane_id: u64,
        out_tx: tokio::sync::mpsc::Sender<(u64, Vec<u8>)>,
        exit_tx: tokio::sync::mpsc::Sender<u64>,
    ) -> Result<PtyShell, PtyError> {
        let pair = open_pty(rows, cols)?;
        let mut errors = Vec::new();
        let mut child = None;
        let mut shell_rc = None;
        for cand in candidates {
            let mut cmd = base_command(cand, cwd, session, pane_id);
            let rc = apply_shell_integration(&mut cmd, cand, session, pane_id);
            match pair.slave.spawn_command(cmd) {
                Ok(c) => {
                    child = Some(c);
                    shell_rc = rc;
                    break;
                }
                // rc drops here on failure -> its temp dir is removed.
                Err(e) => errors.push(format!("{}: {e}", cand.to_string_lossy())),
            }
        }
        let child = child.ok_or_else(|| PtyError::Spawn(errors.join("; ")))?;
        wire(pair, child, pane_id, out_tx, exit_tx, shell_rc)
    }

    /// Spawn an explicit command (`argv[0]` + args) as a pane - the
    /// `fno mux pane run` / agents-spawn primitive. No shell fallback: an
    /// unspawnable argv is the caller's error, surfaced verbatim. Env + cwd
    /// (incl. `FNO_SESSION`) match [`PtyShell::spawn`].
    pub fn spawn_cmd(
        argv: &[String],
        rows: u16,
        cols: u16,
        cwd: Option<&std::path::Path>,
        session: &str,
        pane_id: u64,
        out_tx: tokio::sync::mpsc::Sender<(u64, Vec<u8>)>,
        exit_tx: tokio::sync::mpsc::Sender<u64>,
    ) -> Result<PtyShell, PtyError> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| PtyError::Spawn("empty argv".into()))?;
        let pair = open_pty(rows, cols)?;
        let mut cmd = base_command(OsStr::new(program), cwd, session, pane_id);
        // A bare shell argv (`pane run -- zsh`) is wrapped; a non-shell argv (an
        // agent TUI) passes through untouched. Wrap before the caller's args so
        // bash's `--rcfile` leads.
        let shell_rc = apply_shell_integration(&mut cmd, OsStr::new(program), session, pane_id);
        for a in args {
            cmd.arg(a);
        }
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| PtyError::Spawn(format!("{program}: {e}")))?;
        wire(pair, child, pane_id, out_tx, exit_tx, shell_rc)
    }

    /// The child's OS process id, when the platform reported one. Read for
    /// `pane ls`; `None` never means "dead" (use [`PtyShell::is_child_alive`]).
    pub fn child_pid(&self) -> Option<u32> {
        self.child.lock().ok().and_then(|c| c.process_id())
    }

    /// Queue keystrokes for the child (the writer thread performs the actual
    /// blocking write, so this never blocks the caller). The caller decides
    /// what a failure means (the server drops input fail-closed once the
    /// child has exited, AC2-ERR); a full queue drops the chunk the same way.
    pub fn write_input(&self, bytes: &[u8]) -> Result<(), PtyError> {
        use std::sync::mpsc::TrySendError;
        self.input_tx.try_send(bytes.to_vec()).map_err(|e| match e {
            TrySendError::Full(_) => PtyError::Write(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "pty input queue full; chunk dropped",
            )),
            TrySendError::Disconnected(_) => PtyError::Write(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "pty writer thread gone (child exited)",
            )),
        })
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

    /// Kill and reap the child (explicit ClosePane / CloseTab). Idempotent:
    /// killing an already-dead child errors harmlessly and the wait reaps
    /// either way, so a close racing a natural exit never double-reaps or
    /// leaves a zombie. SIGKILL makes the post-kill wait effectively
    /// immediate, so this is safe on the core loop.
    pub fn kill(&self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Open a fresh PTY pair at `rows`x`cols` (clamped to >=1).
fn open_pty(rows: u16, cols: u16) -> Result<portable_pty::PtyPair, PtyError> {
    native_pty_system()
        .openpty(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| PtyError::OpenPty(e.to_string()))
}

/// A `CommandBuilder` for `program` carrying the pane environment: a login-ish
/// `TERM`, `FNO_SESSION` (AC3-HP: the nested-attach guard and kill-server both
/// read it), `FNO_PANE` (4a-G2: the pane's own id, so an agent hosted in a
/// pane can name itself to the registry and an in-pane spawn can address its
/// host), and the launch cwd when it is still a directory.
fn base_command(
    program: &OsStr,
    cwd: Option<&std::path::Path>,
    session: &str,
    pane_id: u64,
) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(program);
    cmd.env("TERM", "xterm-256color");
    cmd.env("FNO_SESSION", session);
    cmd.env("FNO_PANE", pane_id.to_string());
    if let Some(dir) = cwd.filter(|d| d.is_dir()) {
        cmd.cwd(dir);
    }
    cmd
}

/// A shell the mux knows how to inject OSC 133 block markers into.
enum ShellKind {
    Zsh,
    Bash,
}

/// Map the basename of `program` to a known shell. `None` for an agent TUI or
/// any non-shell argv, which passes through un-wrapped.
fn shell_kind(program: &OsStr) -> Option<ShellKind> {
    match std::path::Path::new(program).file_name()?.to_str()? {
        "zsh" => Some(ShellKind::Zsh),
        "bash" => Some(ShellKind::Bash),
        _ => None,
    }
}

/// The knob's only off-switch: `FNO_MUX_SHELL_INTEGRATION=off`. Anything else
/// (absent, `mux-panes`, garbage) reads as on - the feature's default.
fn integration_disabled(knob: Option<&OsStr>) -> bool {
    knob == Some(OsStr::new("off"))
}

/// The temp `.zshenv`: source the user's real `.zshenv` by explicit path, and
/// deliberately do NOT restore `ZDOTDIR` (that stays pointed at the temp dir so
/// zsh reads OUR `.zshrc` next, not the user's).
const ZSH_ZSHENV: &str =
    "[ -f \"${USER_ZDOTDIR:-$HOME}/.zshenv\" ] && . \"${USER_ZDOTDIR:-$HOME}/.zshenv\"\n";

/// The temp `.zshrc`: restore `ZDOTDIR` for the user rc + subshells, source the
/// user's real `.zshrc`, then eval the snippet LAST so it wins the prompt.
fn zsh_zshrc() -> String {
    format!(
        "ZDOTDIR=\"${{USER_ZDOTDIR:-$HOME}}\"\n\
         [ -f \"$ZDOTDIR/.zshrc\" ] && . \"$ZDOTDIR/.zshrc\"\n\
         {ZSH_SHELL_INIT}"
    )
}

/// The temp bash `--rcfile`: source the user's `~/.bashrc` then the snippet.
fn bash_rcfile_body() -> String {
    format!("[ -f \"$HOME/.bashrc\" ] && . \"$HOME/.bashrc\"\n{BASH_SHELL_INIT}")
}

/// A per-pane temp dir holding the shell-integration rc file(s). Held by the
/// owning `PtyShell` only for cleanup: the rc is read once at shell startup, so
/// it just has to outlive the child's first few ms, which the pane always does.
/// Dropped -> removed when the pane closes / the server exits.
struct ShellRc {
    dir: PathBuf,
}

impl Drop for ShellRc {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Inject the OSC 133 snippet into a mux-spawned shell, and ONLY that shell -
/// never the user's global rc. zsh: a temp `ZDOTDIR` whose `.zshenv` /
/// `.zshrc` source the user's real files (`USER_ZDOTDIR`, or `$HOME` when
/// unset) then eval the snippet LAST, so a prompt owner (starship/p10k) runs
/// first and our `B` marker is appended after. bash: a temp `--rcfile` that
/// sources `~/.bashrc` then the snippet. The snippet's `_FNO_OSC133` guard makes
/// a shell that already emits OSC 133 a no-op, and the scanner is
/// emitter-agnostic. Off when `FNO_MUX_SHELL_INTEGRATION=off`; absent reads as
/// on (`mux-panes`). Fail-open: a temp-write error skips integration rather than
/// failing the pane spawn.
///
/// ponytail: a user whose own `.zshenv` sets `ZDOTDIR` defeats the temp-dir hop
/// (zsh then reads their `.zshrc`, not ours) - the same known limit as VS Code's
/// scheme; the manual `fno mux shell-init` eval is the upgrade path there.
fn apply_shell_integration(
    cmd: &mut CommandBuilder,
    program: &OsStr,
    session: &str,
    pane_id: u64,
) -> Option<ShellRc> {
    if integration_disabled(std::env::var_os("FNO_MUX_SHELL_INTEGRATION").as_deref()) {
        return None;
    }
    let kind = shell_kind(program)?;
    // Under the per-user 0700 mux dir, NOT world-writable /tmp: a shell that
    // sources these rc files is an RCE surface, so a predictable path in a
    // shared temp dir (where an attacker could pre-create the dir and swap the
    // rc) is CWE-377. `ensure_private_dir` forces 0700 on both levels, and no
    // other uid can enter the parent, so the per-pane name being predictable is
    // safe. Unique per pane (session + id); a crashed server's leftover of the
    // same name is removed first, never appended to.
    let dir = crate::proto::mux_dir()
        .join("shell-rc")
        .join(format!("fno-mux-{session}-{pane_id}"));
    let _ = fs::remove_dir_all(&dir);
    crate::proto::ensure_private_dir(&dir).ok()?;
    let rc = ShellRc { dir };
    match kind {
        ShellKind::Zsh => {
            fs::write(rc.dir.join(".zshenv"), ZSH_ZSHENV).ok()?;
            fs::write(rc.dir.join(".zshrc"), zsh_zshrc()).ok()?;
            cmd.env("ZDOTDIR", &rc.dir);
            // Preserve the user's real ZDOTDIR for the temp rc to source; unset
            // -> the in-shell `${USER_ZDOTDIR:-$HOME}` falls back to $HOME.
            if let Some(z) = std::env::var_os("ZDOTDIR") {
                cmd.env("USER_ZDOTDIR", z);
            }
        }
        ShellKind::Bash => {
            let rcfile = rc.dir.join("bashrc");
            fs::write(&rcfile, bash_rcfile_body()).ok()?;
            cmd.arg("--rcfile");
            cmd.arg(rcfile);
        }
    }
    Some(rc)
}

/// Wire a spawned child's PTY into the mux: drop the slave (only the child
/// holds it), start the reader + writer threads, and hand back the owning
/// [`PtyShell`]. Shared by [`PtyShell::spawn`] and [`PtyShell::spawn_cmd`].
fn wire(
    pair: portable_pty::PtyPair,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    pane_id: u64,
    out_tx: tokio::sync::mpsc::Sender<(u64, Vec<u8>)>,
    exit_tx: tokio::sync::mpsc::Sender<u64>,
    shell_rc: Option<ShellRc>,
) -> Result<PtyShell, PtyError> {
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
    spawn_reader(reader, pane_id, out_tx, exit_tx)?;
    let input_tx = spawn_writer(writer)?;
    Ok(PtyShell {
        master: pair.master,
        input_tx,
        child: Mutex::new(child),
        _shell_rc: shell_rc,
    })
}

/// The blocking writer thread: bounded channel -> PTY master. Owns the writer
/// so its blocking `write_all`/`flush` never runs on the tokio core loop
/// (gemini high: a full kernel input buffer + a child blocked on output is a
/// deadlock if the core loop does the write). Exits when the channel closes
/// (PtyShell dropped) or a write fails (child gone).
fn spawn_writer(
    mut writer: Box<dyn Write + Send>,
) -> Result<std::sync::mpsc::SyncSender<Vec<u8>>, PtyError> {
    // 256 keystroke-sized chunks of headroom; beyond that the child has
    // stopped reading input for a long while and dropping is the safe answer.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(256);
    std::thread::Builder::new()
        .name("fno-mux-pty-writer".into())
        .spawn(move || {
            while let Ok(bytes) = rx.recv() {
                if writer
                    .write_all(&bytes)
                    .and_then(|()| writer.flush())
                    .is_err()
                {
                    break; // child/master gone; senders see Disconnected
                }
            }
        })
        .map_err(|e| PtyError::Writer(format!("writer thread spawn failed: {e}")))?;
    Ok(tx)
}

/// The blocking reader thread: PTY master -> the shared pane-tagged channel.
/// Exits on EOF (child gone; macOS reports Ok(0), Linux EIO) or a closed
/// channel, sending the pane id on `exit_tx` either way so the core loop
/// learns WHICH pane's child ended.
fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    pane_id: u64,
    out_tx: tokio::sync::mpsc::Sender<(u64, Vec<u8>)>,
    exit_tx: tokio::sync::mpsc::Sender<u64>,
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
                        if out_tx.blocking_send((pane_id, buf[..n].to_vec())).is_err() {
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
            let _ = exit_tx.blocking_send(pane_id);
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
            shell_candidates(Some(OsStr::new("/bin/zsh"))),
            vec![OsString::from("/bin/zsh"), OsString::from("/bin/sh")]
        );
        assert_eq!(shell_candidates(None), vec![OsString::from("/bin/sh")]);
        assert_eq!(
            shell_candidates(Some(OsStr::new(""))),
            vec![OsString::from("/bin/sh")]
        );
        assert_eq!(
            shell_candidates(Some(OsStr::new("  "))),
            vec![OsString::from("/bin/sh")]
        );
        // No duplicate when $SHELL already is /bin/sh.
        assert_eq!(
            shell_candidates(Some(OsStr::new("/bin/sh"))),
            vec![OsString::from("/bin/sh")]
        );
    }

    #[test]
    fn shell_integration_detects_only_known_shells() {
        // basename maps zsh/bash (bare or full-path); everything else is a
        // non-shell argv that passes through un-wrapped.
        assert!(matches!(
            shell_kind(OsStr::new("zsh")),
            Some(ShellKind::Zsh)
        ));
        assert!(matches!(
            shell_kind(OsStr::new("/bin/zsh")),
            Some(ShellKind::Zsh)
        ));
        assert!(matches!(
            shell_kind(OsStr::new("/usr/local/bin/bash")),
            Some(ShellKind::Bash)
        ));
        assert!(shell_kind(OsStr::new("claude")).is_none());
        assert!(shell_kind(OsStr::new("/usr/bin/nvim")).is_none());
        assert!(shell_kind(OsStr::new("fish")).is_none());
    }

    #[test]
    fn shell_integration_off_knob_only_matches_off() {
        // Only the literal "off" disables; absent / mux-panes / garbage read on.
        assert!(integration_disabled(Some(OsStr::new("off"))));
        assert!(!integration_disabled(None));
        assert!(!integration_disabled(Some(OsStr::new("mux-panes"))));
        assert!(!integration_disabled(Some(OsStr::new(""))));
        assert!(!integration_disabled(Some(OsStr::new("on"))));
    }

    #[test]
    fn shell_integration_rc_embeds_snippet_and_sources_user_rc() {
        // zsh: restore ZDOTDIR first, source the user's .zshrc, snippet LAST.
        let zshrc = zsh_zshrc();
        assert!(zshrc.starts_with("ZDOTDIR=\"${USER_ZDOTDIR:-$HOME}\""));
        assert!(zshrc.contains("$ZDOTDIR/.zshrc"));
        assert!(zshrc.contains("_FNO_OSC133"));
        assert!(zshrc.trim_end().ends_with(ZSH_SHELL_INIT.trim_end()));
        // .zshenv must NOT restore ZDOTDIR (keeps zsh reading our .zshrc next).
        assert!(!ZSH_ZSHENV.contains("ZDOTDIR="));
        assert!(ZSH_ZSHENV.contains(".zshenv"));
        // bash: source the user's ~/.bashrc then the snippet.
        let bashrc = bash_rcfile_body();
        assert!(bashrc.contains("$HOME/.bashrc"));
        assert!(bashrc.contains("_FNO_OSC133"));
    }

    #[tokio::test]
    async fn server_spine_unspawnable_shell_falls_back() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let (exit_tx, _exit_rx) = tokio::sync::mpsc::channel(4);
        let candidates = shell_candidates(Some(OsStr::new("/nonexistent/definitely-not-a-shell")));
        let shell = PtyShell::spawn(&candidates, 24, 80, None, "main", 7, tx, exit_tx)
            .expect("fallback must spawn");
        assert!(shell.is_child_alive());
        // Prove the fallback shell is real: round-trip a command, and assert
        // every chunk carries this pane's tag.
        shell.write_input(b"echo fallback-ok\r").unwrap();
        let mut seen = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
                Ok(Some((pane_id, chunk))) => {
                    assert_eq!(pane_id, 7, "reader must tag output with its pane id");
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

    #[tokio::test]
    async fn server_spine_child_exit_signals_the_exit_channel() {
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let (exit_tx, mut exit_rx) = tokio::sync::mpsc::channel(4);
        let shell = PtyShell::spawn(
            &shell_candidates(Some(OsStr::new("/bin/sh"))),
            24,
            80,
            None,
            "main",
            42,
            tx,
            exit_tx,
        )
        .expect("spawns");
        shell.write_input(b"exit\r").unwrap();
        let pid = tokio::time::timeout(std::time::Duration::from_secs(10), exit_rx.recv())
            .await
            .expect("exit must be signaled")
            .expect("channel open");
        assert_eq!(pid, 42, "exit signal must carry the pane id");
    }

    #[tokio::test]
    async fn server_spine_pane_env_carries_session_and_pane_id() {
        // 4a-G2: FNO_PANE joins FNO_SESSION in every pane child env, so a
        // hosted agent can name its own pane and an in-pane spawn inherits
        // the session.
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let (exit_tx, _exit_rx) = tokio::sync::mpsc::channel(4);
        let shell = PtyShell::spawn(
            &shell_candidates(Some(OsStr::new("/bin/sh"))),
            24,
            80,
            None,
            "envtest",
            31,
            tx,
            exit_tx,
        )
        .expect("spawns");
        shell
            .write_input(b"echo mark-$FNO_SESSION-$FNO_PANE-end\r")
            .unwrap();
        let mut seen = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
                Ok(Some((_, chunk))) => {
                    seen.extend_from_slice(&chunk);
                    if String::from_utf8_lossy(&seen).contains("mark-envtest-31-end") {
                        return;
                    }
                }
                Ok(None) => break,
                Err(_) => {}
            }
        }
        panic!(
            "pane env vars never echoed: {:?}",
            String::from_utf8_lossy(&seen)
        );
    }

    #[test]
    fn server_spine_all_candidates_unspawnable_is_a_clear_error() {
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let (exit_tx, _exit_rx) = tokio::sync::mpsc::channel(4);
        let err = PtyShell::spawn(
            &["/nonexistent/a".into(), "/nonexistent/b".into()],
            24,
            80,
            None,
            "main",
            0,
            tx,
            exit_tx,
        )
        .err()
        .expect("must fail");
        let msg = err.to_string();
        assert!(msg.contains("/nonexistent/a"), "{msg}");
        assert!(msg.contains("/nonexistent/b"), "{msg}");
    }
}
