//! The mux client: a dumb compositor over the server's frames.
//!
//! Takes over the terminal (crossterm raw mode + alternate screen), attaches
//! to the session server (spawning one if absent), draws `Frame`s, and
//! forwards stdin to the server. Input fidelity is by construction: in raw
//! mode the client reads RAW STDIN BYTES and forwards them verbatim on the
//! reliable channel - Ctrl-C, arrow keys, and UTF-8 are never re-encoded
//! (AC2-UI). The client never emulates VT itself; the server grid is the
//! single source of truth, which is what makes reattach exact.
//!
//! Detach: Ctrl-\ (byte 0x1C). ponytail: a bare byte-match means a 0x1C
//! inside a paste also detaches; a prefix-key state machine is the Phase-3
//! upgrade if that ever bites.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::style::Color as CtColor;
use crossterm::{cursor, queue, style, terminal};
use tokio::sync::mpsc;

use crate::proto::{
    self, read_msg, write_msg, Cell, ClientMsg, Color, Frame, ProtoError, ServerMsg, BUILD_VERSION,
    PROTO_VERSION,
};

/// Ctrl-\ : detach from the session, leaving the server (and shell) running.
const DETACH_BYTE: u8 = 0x1C;

/// How long to wait for a just-spawned server to accept.
const SPAWN_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Run the client for `session`. Returns the process exit code.
pub fn run(session: &str) -> i32 {
    match run_inner(session) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("fno: {e}");
            1
        }
    }
}

fn run_inner(session: &str) -> Result<i32, String> {
    proto::ensure_mux_dir().map_err(|e| format!("cannot prepare the mux dir: {e}"))?;
    let path = proto::socket_path(session)?;
    let stream = connect_or_spawn(&path)?;

    let runtime = tokio::runtime::Runtime::new().map_err(|e| format!("runtime: {e}"))?;
    runtime.block_on(attach_and_run(stream, &path))
}

/// Connect to a live server, or spawn one and connect. AC3-ERR: a dead
/// server's stale socket gets a one-line notice and a fresh server - never a
/// hang on a dead socket (the spawned server's bind unlinks it).
fn connect_or_spawn(path: &Path) -> Result<std::os::unix::net::UnixStream, String> {
    if let Ok(s) = std::os::unix::net::UnixStream::connect(path) {
        return Ok(s);
    }
    if path.exists() {
        eprintln!("fno: previous session ended; starting a fresh one");
    }
    spawn_server(path)?;
    let deadline = Instant::now() + SPAWN_CONNECT_TIMEOUT;
    loop {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(s) => return Ok(s),
            Err(e) if Instant::now() >= deadline => {
                return Err(format!(
                    "server did not come up at {} ({e}); check {}",
                    path.display(),
                    log_path(path).display()
                ));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(30)),
        }
    }
}

fn log_path(socket: &Path) -> PathBuf {
    socket.with_extension("log")
}

/// Spawn `fno --server <socket>` detached: its own session (setsid) so the
/// server never receives the terminal's SIGHUP, stderr to a per-session log.
/// Two clients racing here both spawn; the bind is the lock, the losing
/// server exits 0, and both clients attach to the winner (AC4-EDGE).
fn spawn_server(path: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find own binary: {e}"))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(path))
        .map_err(|e| format!("cannot open server log: {e}"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--server")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log);
    // Safety: setsid only detaches the child from our session/terminal; it is
    // async-signal-safe and touches no shared state.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("cannot spawn the mux server: {e}"))
}

/// Restore the terminal on every exit path, including panics.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self, String> {
        terminal::enable_raw_mode().map_err(|e| format!("raw mode: {e}"))?;
        let mut out = std::io::stdout();
        // Surface an alt-screen failure instead of silently painting over the
        // user's scrollback. The guard exists from here, so raw mode is
        // restored by Drop on the error path.
        let guard = TerminalGuard;
        crossterm::execute!(out, terminal::EnterAlternateScreen)
            .map_err(|e| format!("alternate screen: {e}"))?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        let _ = crossterm::execute!(out, terminal::LeaveAlternateScreen, cursor::Show);
        let _ = terminal::disable_raw_mode();
    }
}

async fn attach_and_run(
    stream: std::os::unix::net::UnixStream,
    socket: &Path,
) -> Result<i32, String> {
    // A server that dies between accept and Attach (e.g. no spawnable shell)
    // closes the connection without a reason; its stderr has the real cause.
    let log_hint = format!("check {}", log_path(socket).display());
    stream
        .set_nonblocking(true)
        .map_err(|e| format!("socket setup: {e}"))?;
    let stream = tokio::net::UnixStream::from_std(stream).map_err(|e| format!("socket: {e}"))?;
    let (mut sock_r, mut sock_w) = stream.into_split();

    let (cols, rows) = terminal::size().map_err(|e| format!("terminal size: {e}"))?;
    // The launch cwd keys squad selection server-side (squad.rs). An
    // unreadable cwd (deleted directory) degrades to "" - the server treats
    // it as a literal-path squad, never a refused attach.
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    write_msg(
        &mut sock_w,
        &ClientMsg::Attach {
            proto: PROTO_VERSION,
            build: BUILD_VERSION.to_string(),
            rows,
            cols,
            cwd,
        },
    )
    .await
    .map_err(|e| format!("attach failed: {e}"))?;

    // The first frame (or refusal) decides everything, BEFORE the terminal
    // is taken over, so a refusal prints as a plain one-liner (AC1-ERR,
    // version skew). The v2 server sends a reliable preamble ahead of it
    // (ModeSync/Layout/Notice); this Phase-1-shaped client skips those - the
    // N-pane compositor (task 2.5) renders them.
    let deadline = Instant::now() + Duration::from_secs(10);
    let first_frame = loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| format!("server did not answer the attach; {log_hint}"))?;
        let first = tokio::time::timeout(remaining, read_msg::<_, ServerMsg>(&mut sock_r))
            .await
            .map_err(|_| format!("server did not answer the attach; {log_hint}"))?;
        match first {
            Ok(ServerMsg::Frame { frame, .. }) => break checked_frame(frame)?,
            Ok(ServerMsg::Bye { reason }) => return Err(reason),
            Ok(ServerMsg::Layout { .. })
            | Ok(ServerMsg::ModeSync { .. })
            | Ok(ServerMsg::Notice { .. }) => continue,
            Err(e) => return Err(format!("attach failed: {e}; {log_hint}")),
        }
    };

    // Socket reads get their own task. `read_msg` is NOT cancellation-safe
    // (a select! that drops it between the length prefix and the body loses
    // the consumed bytes and desyncs the whole stream), so the select loop
    // below must never poll it directly - it drains this channel instead,
    // and mpsc recv IS cancel-safe.
    let (srv_tx, mut srv_rx) = mpsc::channel::<Result<ServerMsg, ProtoError>>(16);
    tokio::spawn(async move {
        loop {
            let msg = read_msg::<_, ServerMsg>(&mut sock_r).await;
            let is_err = msg.is_err();
            if srv_tx.send(msg).await.is_err() || is_err {
                break;
            }
        }
    });

    // Raw stdin -> channel; forwarded verbatim below.
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(64);
    std::thread::Builder::new()
        .name("fno-mux-stdin".into())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 4096];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if stdin_tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })
        .map_err(|e| format!("stdin thread: {e}"))?;

    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
        .map_err(|e| format!("signal setup: {e}"))?;

    let guard = TerminalGuard::enter()?;
    let mut compositor = Compositor::new();
    compositor
        .draw(&first_frame)
        .map_err(|e| format!("draw: {e}"))?;

    let exit: Result<i32, String> = loop {
        tokio::select! {
            msg = srv_rx.recv() => match msg.unwrap_or(Err(ProtoError::Closed)) {
                Ok(ServerMsg::Frame { frame, .. }) => {
                    let f = match checked_frame(frame) {
                        Ok(f) => f,
                        Err(e) => break Err(e),
                    };
                    if let Err(e) = compositor.draw(&f) {
                        break Err(format!("draw: {e}"));
                    }
                }
                // Layout/ModeSync/Notice render in the N-pane compositor
                // (task 2.5). The Phase-1-shaped server never sends them;
                // ignoring instead of erroring keeps mixed-task states sane.
                Ok(ServerMsg::Layout { .. })
                | Ok(ServerMsg::ModeSync { .. })
                | Ok(ServerMsg::Notice { .. }) => {}
                Ok(ServerMsg::Bye { reason }) => break Ok(exit_with_notice(reason)),
                Err(ProtoError::Closed) => {
                    break Ok(exit_with_notice("session ended (server closed)".into()));
                }
                Err(e) => break Err(format!("connection lost: {e}")),
            },
            bytes = stdin_rx.recv() => match bytes {
                Some(bytes) => {
                    if let Some(pos) = bytes.iter().position(|&b| b == DETACH_BYTE) {
                        // Forward what was typed before the detach key, then go.
                        if pos > 0 {
                            let _ = write_msg(&mut sock_w, &ClientMsg::Input(bytes[..pos].to_vec())).await;
                        }
                        let _ = write_msg(&mut sock_w, &ClientMsg::Detach).await;
                        break Ok(exit_with_notice("detached; run fno to reattach".into()));
                    }
                    // Reliable channel: awaited send, input is NEVER dropped.
                    if let Err(e) = write_msg(&mut sock_w, &ClientMsg::Input(bytes)).await {
                        break Err(format!("input send failed: {e}"));
                    }
                }
                // The stdin thread breaks on EOF and on read error alike; by
                // the time we see None we cannot tell which, so say so.
                None => break Ok(exit_with_notice("stdin ended (closed or read error); detached".into())),
            },
            _ = winch.recv() => {
                if let Ok((cols, rows)) = terminal::size() {
                    // The server resizes PTY + grid and broadcasts a fresh
                    // frame; the size change makes the compositor full-redraw.
                    if let Err(e) = write_msg(&mut sock_w, &ClientMsg::Resize { rows, cols }).await {
                        break Err(format!("resize send failed: {e}"));
                    }
                }
            }
        }
    };
    drop(guard); // restore the terminal BEFORE printing the notice
    match exit {
        Ok(code) => {
            if let Some(n) = NOTICE.with(|n| n.borrow_mut().take()) {
                eprintln!("fno: {n}");
            }
            Ok(code)
        }
        Err(e) => Err(e),
    }
}

// The exit notice must print AFTER the alternate screen is left, or it is
// erased with the TUI. Thread-local because the select loop returns through
// several arms; a struct field would work too but this stays local to the file.
thread_local! {
    static NOTICE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

fn exit_with_notice(notice: String) -> i32 {
    NOTICE.with(|n| *n.borrow_mut() = Some(notice));
    0
}

/// The wire trust boundary: a `Frame` whose cell count disagrees with its
/// geometry would panic the compositor's slice math. Reject it like a
/// malformed message - close loudly, never draw (same posture as `read_msg`).
fn checked_frame(f: Frame) -> Result<Frame, String> {
    if f.geometry_ok() {
        Ok(f)
    } else {
        Err(format!(
            "malformed frame from server: {}x{} but {} cells",
            f.rows,
            f.cols,
            f.cells.len()
        ))
    }
}

/// Draws frames with a row-level diff against what was actually drawn last -
/// safe precisely because it diffs against its own output, never against a
/// prediction of server state.
struct Compositor {
    last: Option<Frame>,
}

impl Compositor {
    fn new() -> Self {
        Compositor { last: None }
    }

    fn draw(&mut self, frame: &Frame) -> std::io::Result<()> {
        let mut out = std::io::stdout().lock();
        let full = match &self.last {
            Some(prev) => prev.rows != frame.rows || prev.cols != frame.cols,
            None => true,
        };
        if full {
            queue!(out, terminal::Clear(terminal::ClearType::All))?;
        }
        queue!(out, cursor::Hide)?;
        for r in 0..frame.rows as usize {
            if !full {
                // Row unchanged since we drew it? Skip the write entirely.
                let prev = self.last.as_ref().unwrap();
                let w = frame.cols as usize;
                if prev.cells[r * w..(r + 1) * w] == frame.cells[r * w..(r + 1) * w] {
                    continue;
                }
            }
            self.draw_row(&mut out, frame, r)?;
        }
        queue!(out, cursor::MoveTo(frame.cursor_col, frame.cursor_row))?;
        if frame.cursor_visible {
            queue!(out, cursor::Show)?;
        } else {
            queue!(out, cursor::Hide)?;
        }
        out.flush()?;
        self.last = Some(frame.clone());
        Ok(())
    }

    fn draw_row(&self, out: &mut impl Write, frame: &Frame, r: usize) -> std::io::Result<()> {
        queue!(out, cursor::MoveTo(0, r as u16))?;
        let w = frame.cols as usize;
        let mut style_of: Option<(Color, Color, u8)> = None;
        for cell in &frame.cells[r * w..(r + 1) * w] {
            if cell.flags & proto::cell_flags::WIDE_SPACER != 0 {
                continue; // the wide glyph before it already covers this column
            }
            let key = (cell.fg, cell.bg, cell.flags);
            if style_of != Some(key) {
                apply_style(out, cell)?;
                style_of = Some(key);
            }
            queue!(out, style::Print(cell.c))?;
        }
        // Leave the line in a reset state so scrolling artifacts never bleed.
        queue!(out, style::SetAttribute(style::Attribute::Reset))?;
        Ok(())
    }
}

fn apply_style(out: &mut impl Write, cell: &Cell) -> std::io::Result<()> {
    use proto::cell_flags as cf;
    // Reset first: attribute REMOVAL (e.g. bold -> plain) has no incremental
    // form worth tracking at this scale.
    queue!(out, style::SetAttribute(style::Attribute::Reset))?;
    if cell.flags & cf::BOLD != 0 {
        queue!(out, style::SetAttribute(style::Attribute::Bold))?;
    }
    if cell.flags & cf::ITALIC != 0 {
        queue!(out, style::SetAttribute(style::Attribute::Italic))?;
    }
    if cell.flags & cf::UNDERLINE != 0 {
        queue!(out, style::SetAttribute(style::Attribute::Underlined))?;
    }
    if cell.flags & cf::INVERSE != 0 {
        queue!(out, style::SetAttribute(style::Attribute::Reverse))?;
    }
    if cell.flags & cf::DIM != 0 {
        queue!(out, style::SetAttribute(style::Attribute::Dim))?;
    }
    queue!(
        out,
        style::SetForegroundColor(map_color(cell.fg, true)),
        style::SetBackgroundColor(map_color(cell.bg, false))
    )?;
    Ok(())
}

fn map_color(c: Color, _fg: bool) -> CtColor {
    match c {
        Color::Default => CtColor::Reset,
        Color::Indexed(i) => CtColor::AnsiValue(i),
        Color::Rgb(r, g, b) => CtColor::Rgb { r, g, b },
    }
}
