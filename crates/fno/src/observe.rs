//! `fno mux serve --observe --pane <id>`: watch one pane from a terminal, read
//! only. Run it over ssh: `ssh -t host fno mux serve --observe --pane 3`.
//!
//! The observer attaches the way the web bridge does (`Attach { rows: 0, cols:
//! 0 }`, write half dropped), then tags itself with a name and the pane so the
//! driver sees a count on the pane border and a line on the sideline. Its
//! frames pass through the same mask as the browser share before they reach
//! this terminal. Keys are read only to catch `q`; no byte typed here goes
//! anywhere.

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::proto::{self, Frame, ObserverHello, ObserverVia, ServerMsg};
use crate::server::observers::clean_label;
use crate::{frame_ansi, share, web};

/// Parsed `fno mux serve --observe` arguments.
#[derive(Debug, PartialEq, Eq)]
pub struct ObserveArgs {
    pub session: String,
    pub pane: u64,
    pub name: Option<String>,
}

/// `user from <ssh client address>` over ssh, else the user name alone.
fn default_name() -> String {
    let user = std::env::var("USER").unwrap_or_default();
    match std::env::var("SSH_CONNECTION")
        .ok()
        .and_then(|c| c.split_whitespace().next().map(str::to_owned))
    {
        Some(from) => format!("{user} from {from}"),
        None => user,
    }
}

/// Whether a chunk of typed bytes asks to leave: `q`, Ctrl-C or Ctrl-D.
fn quits(bytes: &[u8]) -> bool {
    bytes.iter().any(|b| matches!(b, b'q' | b'Q' | 0x03 | 0x04))
}

/// Raw mode and the alternate screen for the life of the watch, undone on
/// every exit path.
struct Screen;

impl Screen {
    fn enter() -> Result<Screen, String> {
        crossterm::terminal::enable_raw_mode().map_err(|e| format!("terminal raw mode: {e}"))?;
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[?1049h\x1b[?25l\x1b[2J");
        let _ = out.flush();
        Ok(Screen)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[0m\x1b[?25h\x1b[?1049l");
        let _ = out.flush();
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Typed bytes, read on a thread because a tty read cannot be cancelled.
fn spawn_keys() -> mpsc::UnboundedReceiver<Vec<u8>> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 64];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });
    rx
}

fn header(pane: u64, name: &str, frame: &Frame) -> String {
    format!(
        "watching pane {pane} ({}x{}) as {name} - q to leave",
        frame.cols, frame.rows
    )
}

fn redraw(frame: &Frame, header: &str, size: (u16, u16), clear: bool) {
    let (cols, rows) = size;
    let mut out = Vec::new();
    if clear {
        out.extend_from_slice(b"\x1b[2J");
    }
    let header = crate::chrome::clip(header, cols as usize);
    out.extend_from_slice(format!("\x1b[1;1H\x1b[0;2m{header}\x1b[0m\x1b[K").as_bytes());
    out.extend(frame_ansi::paint(frame, 1, rows, cols));
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(&out);
    let _ = stdout.flush();
}

/// Entry point for the `mux serve --observe` role; returns the exit code.
pub fn serve(args: ObserveArgs) -> i32 {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!(
            "fno mux serve --observe needs a terminal: over ssh, run it with `ssh -t host ...`"
        );
        return 2;
    }
    let socket = match proto::socket_path(&args.session) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno mux serve --observe: {e}");
            return 2;
        }
    };
    let identity = match share::list_panes(&socket, &args.session) {
        Ok(panes) => match panes.iter().find(|p| p.pane_id == args.pane) {
            Some(p) => share::pane_identity(p),
            None => {
                eprintln!(
                    "fno mux serve --observe: no pane {} in session {:?}; list panes with `fno mux pane ls`",
                    args.pane, args.session
                );
                return 2;
            }
        },
        Err(e) => {
            eprintln!("fno mux serve --observe: {e}");
            return 2;
        }
    };
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fno mux serve --observe: cannot start runtime: {e}");
            return 1;
        }
    };
    let (code, reason) = runtime.block_on(watch(args, socket, identity));
    runtime.shutdown_timeout(Duration::from_millis(500));
    eprintln!("{reason}");
    code
}

/// Watch until the user leaves, the pane closes or the server goes away.
/// Returns the exit code and the reason, printed after the terminal is back.
async fn watch(args: ObserveArgs, socket: PathBuf, identity: String) -> (i32, String) {
    let name = match clean_label(&args.name.clone().unwrap_or_else(default_name)) {
        n if n.is_empty() => "observer".to_string(),
        n => n,
    };
    let hello = ObserverHello {
        name: name.clone(),
        pane: args.pane,
        via: ObserverVia::Ssh,
        frames: true,
    };
    let (mut reader, first) = match web::connect_attach(&socket, Some(hello)).await {
        Ok(v) => v,
        Err(e) => return (1, e),
    };
    // Frames are read on their own task: a framed read is not cancel-safe, so
    // it must never sit inside a select.
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMsg>();
    tokio::spawn(async move {
        while let Ok(msg) = proto::read_msg::<_, ServerMsg>(&mut reader).await {
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
    let _screen = match Screen::enter() {
        Ok(s) => s,
        Err(e) => return (1, e),
    };
    let mut keys = spawn_keys();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let size = || crossterm::terminal::size().unwrap_or((80, 24));
    let mut term = size();
    let mut latest: Option<Frame> = None;
    let mut next = Some(first);
    let mut recheck = false;
    loop {
        if let Some(msg) = next.take() {
            match msg {
                ServerMsg::Bye { reason } => {
                    return (1, format!("the mux server ended the watch: {reason}"))
                }
                ServerMsg::Layout { .. } => recheck = true,
                other => {
                    if let Some(frame) = share::masked_pane_frame(other, args.pane) {
                        redraw(&frame, &header(args.pane, &name, &frame), term, false);
                        latest = Some(frame);
                    }
                }
            }
        }
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(m) => next = Some(m),
                None => return (1, "the mux server went away".to_string()),
            },
            typed = keys.recv() => match typed {
                Some(bytes) if quits(&bytes) => return (0, "left the watch".to_string()),
                Some(_) => {}
                None => return (0, "terminal closed".to_string()),
            },
            _ = tick.tick() => {
                if std::mem::take(&mut recheck) {
                    let (sock, session) = (socket.clone(), args.session.clone());
                    let panes = tokio::task::spawn_blocking(move || share::list_panes(&sock, &session)).await;
                    if let Ok(Ok(panes)) = panes {
                        if !share::pane_still_shared(&panes, args.pane, &identity) {
                            return (1, format!("pane {} closed", args.pane));
                        }
                    }
                }
                let now = size();
                if now != term {
                    term = now;
                    if let Some(frame) = &latest {
                        redraw(frame, &header(args.pane, &name, frame), term, true);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_q_and_the_interrupt_keys_leave() {
        assert!(quits(b"q") && quits(b"Q") && quits(&[0x03]) && quits(&[0x04]));
        assert!(!quits(b"\x1b[A"), "an arrow key is not a request to leave");
        assert!(!quits(b"x y\r"));
    }
}
