//! The client's launch moment: take the ground (OSC 11/10/4), draw the
//! splash INLINE on the normal screen so it survives in scrollback, then
//! enter the alternate screen for the UI. Any keypress skips the splash;
//! CI, reduced motion, or a non-TTY sees the last frame once.

use std::io::Write;

use crossterm::{cursor, terminal};
use tokio::sync::mpsc;

use crate::theme::Theme;

/// Restore the terminal on every exit path, including panics and SIGTERM.
pub(super) struct TerminalGuard {
    /// Whether the OSC ground was set: the color restore only emits when the
    /// takeover ran, so the kill switch short-circuits both directions.
    ground_set: bool,
}

/// The emergency terminal restore for the SIGTERM handler, written with a
/// raw `write(2)`: OSC color reset, leave the alternate screen, show the
/// cursor. Async-signal-safe: a fixed byte string, no allocation.
const EMERGENCY_RESTORE: &[u8] = b"\x1b]111\x1b\\\x1b]110\x1b\\\x1b]104\x1b\\\x1b[?1049l\x1b[?25h";

extern "C" fn sigterm_restore(_: libc::c_int) {
    unsafe {
        libc::write(
            1,
            EMERGENCY_RESTORE.as_ptr() as *const libc::c_void,
            EMERGENCY_RESTORE.len(),
        );
    }
    // Die with the expected status: default disposition, then re-raise.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
        libc::raise(libc::SIGTERM);
    }
}

impl TerminalGuard {
    /// Raw mode only: the inline splash draws on the NORMAL screen, before
    /// the alternate screen is taken. The SIGTERM handler installs here so
    /// even a kill during the splash restores the user's colors.
    pub(super) fn enter_raw() -> Result<Self, String> {
        terminal::enable_raw_mode().map_err(|e| format!("raw mode: {e}"))?;
        crate::process_admission::set_terminal_owned(true);
        unsafe {
            libc::signal(libc::SIGTERM, sigterm_restore as *const () as usize);
        }
        Ok(TerminalGuard { ground_set: false })
    }

    /// Latch the color restore for a ground taken AFTER launch (a live theme
    /// switch): the exit Drop and the SIGTERM handler then restore too.
    pub(super) fn latch_ground(&mut self) {
        self.ground_set = true;
    }

    /// Tests cannot construct the guard (the field is private to this
    /// module) and must not enable raw mode to get one.
    #[cfg(test)]
    pub(super) fn test_guard() -> Self {
        TerminalGuard { ground_set: false }
    }

    #[cfg(test)]
    pub(super) fn ground_latched(&self) -> bool {
        self.ground_set
    }

    /// The alternate screen + mouse capture, after the inline splash.
    fn enter_screen(&mut self) -> Result<(), String> {
        let mut out = std::io::stdout();
        // Surface an alt-screen failure instead of silently painting over
        // the user's scrollback; the guard already covers raw mode.
        crossterm::execute!(out, terminal::EnterAlternateScreen)
            .map_err(|e| format!("alternate screen: {e}"))?;
        // Mouse capture stays on for the client's whole life (US1/US2/US3): the
        // server routes every pane-rect event by the pane's live mode. Drop's
        // MODE_RESET (which lists 1000/1002/1006 off) turns it back off on exit.
        out.write_all(crate::mouse::ENABLE)
            .and_then(|_| out.flush())
            .map_err(|e| format!("enable mouse: {e}"))?;
        Ok(())
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
        if self.ground_set {
            let _ = out.write_all(crate::theme::GROUND_RESTORE);
        }
        let _ = out.write_all(MODE_RESET);
        let _ = crossterm::execute!(out, terminal::LeaveAlternateScreen, cursor::Show);
        let _ = terminal::disable_raw_mode();
        crate::process_admission::set_terminal_owned(false);
    }
}

/// Take the ground, draw the splash INLINE on the normal screen, then enter
/// the alternate screen for the UI. Returns the guard: dropping it restores
/// the terminal, the colors included. `ground` is the pre-built OSC set
/// sequence, `None` when the kill switch is off or the theme paints no
/// ground. `tx` is the stdin channel's sender: the splash hands back the
/// bytes that ended it, so no typed-ahead key is lost.
pub(super) async fn begin(
    rx: &mut mpsc::Receiver<Vec<u8>>,
    tx: &mpsc::Sender<Vec<u8>>,
    theme: &Theme,
    stashed_modesync: &[u8],
    ground: Option<&[u8]>,
) -> Result<TerminalGuard, String> {
    let mut guard = TerminalGuard::enter_raw()?;
    if let Some(set) = ground {
        let mut out = std::io::stdout();
        out.write_all(set)
            .and_then(|_| out.flush())
            .map_err(|e| format!("ground set: {e}"))?;
        guard.ground_set = true;
    }
    // The splash draws on the NORMAL screen: what it paints survives in
    // scrollback after fno exits, exactly like a CLI banner.
    crate::splash::run(rx, tx, theme).await;
    guard.enter_screen()?;
    if !stashed_modesync.is_empty() {
        super::raw_out(stashed_modesync).map_err(|e| format!("mode sync: {e}"))?;
    }
    Ok(guard)
}
