//! The client's launch moment: enter the terminal (raw mode + alternate
//! screen + mouse capture), replay any stashed ModeSync state, then draw
//! the launch splash (the animated `Ｆ[no]` mark) before the first UI
//! paint. Any keypress skips it; CI, reduced motion, or a non-TTY sees the
//! last frame once.

use std::io::Write;

use crossterm::{cursor, terminal};
use tokio::sync::mpsc;

use crate::theme::Theme;

/// Restore the terminal on every exit path, including panics.
pub(super) struct TerminalGuard;

impl TerminalGuard {
    pub(super) fn enter() -> Result<Self, String> {
        terminal::enable_raw_mode().map_err(|e| format!("raw mode: {e}"))?;
        let mut out = std::io::stdout();
        // Surface an alt-screen failure instead of silently painting over the
        // user's scrollback. The guard exists from here, so raw mode is
        // restored by Drop on the error path.
        let guard = TerminalGuard;
        crossterm::execute!(out, terminal::EnterAlternateScreen)
            .map_err(|e| format!("alternate screen: {e}"))?;
        // Mouse capture stays on for the client's whole life (US1/US2/US3): the
        // server routes every pane-rect event by the pane's live mode. Drop's
        // MODE_RESET (which lists 1000/1002/1006 off) turns it back off on exit.
        out.write_all(crate::mouse::ENABLE)
            .and_then(|_| out.flush())
            .map_err(|e| format!("enable mouse: {e}"))?;
        Ok(guard)
    }
}

/// Every DEC/private mode `ModeSync` can set, reset. Emitted unconditionally
/// on exit (codex P2): a focused vim's mouse reporting or bracketed paste
/// must never survive onto the user's real terminal after `fno` exits, and
/// tracking exactly-what-was-set buys nothing over resetting the fixed set
/// `vt::mode_diff` can emit. Unknown sequences (kitty CSI-u on a plain
/// terminal) are ignored by terminals by design.
const MODE_RESET: &[u8] =
    b"\x1b[?1l\x1b>\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1007l\x1b[?2004l\x1b[=0;1u";

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        let _ = out.write_all(MODE_RESET);
        let _ = crossterm::execute!(out, terminal::LeaveAlternateScreen, cursor::Show);
        let _ = terminal::disable_raw_mode();
    }
}

/// Enter the terminal, replay stashed ModeSync state, then draw the launch
/// splash. Returns the guard: dropping it restores the terminal. `tx` is
/// the stdin channel's sender: the splash hands back the bytes that ended
/// it, so no typed-ahead key is lost.
pub(super) async fn begin(
    rx: &mut mpsc::Receiver<Vec<u8>>,
    tx: &mpsc::Sender<Vec<u8>>,
    theme: &Theme,
    stashed_modesync: &[u8],
) -> Result<TerminalGuard, String> {
    let guard = TerminalGuard::enter()?;
    if !stashed_modesync.is_empty() {
        super::raw_out(stashed_modesync).map_err(|e| format!("mode sync: {e}"))?;
    }
    crate::splash::run(rx, tx, theme).await;
    Ok(guard)
}
