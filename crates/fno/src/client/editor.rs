//! Suspend the mux around `$EDITOR`: leave the alternate screen and raw
//! mode, run the editor on a file, then restore both on every path.

use std::path::Path;

/// The editor a suspended edit opens, named so the keys table's button can
/// say what runs: `$EDITOR`, or vi when none is set (the spawn default).
pub(super) fn editor_name() -> String {
    std::env::var("EDITOR").unwrap_or_else(|_| "vi".into())
}

/// Run `$EDITOR` (default vi) on `path` with the terminal suspended. True on
/// a zero exit.
pub(super) fn edit_file_suspended(path: &Path) -> bool {
    use crossterm::{cursor, execute, terminal};
    use std::io::Write as _;
    let mut out = std::io::stdout();
    let _ = execute!(out, terminal::LeaveAlternateScreen, cursor::Show);
    let _ = terminal::disable_raw_mode();
    let editor = editor_name();
    // A spawn failure restores the terminal too, or the client keeps
    // running cooked and unpainted.
    let ok = std::process::Command::new(&editor)
        .arg(path)
        .status()
        .is_ok_and(|status| status.success());
    let _ = terminal::enable_raw_mode();
    let _ = execute!(out, terminal::EnterAlternateScreen, cursor::Hide);
    let _ = out.flush();
    ok
}
