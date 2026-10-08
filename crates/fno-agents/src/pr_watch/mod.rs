//! The `pr-watch` verb family's Rust home. `status`, `install` and `refresh`
//! live here first; the other three verbs (heal, uninstall, tick) follow,
//! each one replacing a Python leaf with a working forward until the Python
//! package is gone. One verb per PR, every spelling kept.

mod install;
pub(crate) mod refresh;
mod render;
mod status;

use std::path::Path;

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("refresh") => refresh::run(&args[1..]),
        Some("status") => status::run(&args[1..]),
        Some("install") => install::run(&args[1..]),
        _ => {
            eprintln!("usage: fno-agents pr-watch <status|install|refresh> [--json|-J] [-N]");
            2
        }
    }
}

/// Test seam for the refresh parity fixture: the fixture pre-renders the
/// plist with the renderer itself, so the "unchanged" case exercises the
/// no-change branch against the exact bytes the verb writes.
#[doc(hidden)]
pub fn render_plist_for_test(
    launch_agents_dir: &Path,
    fno_binary: &str,
    install_path: Option<&str>,
    interval: i64,
) -> String {
    refresh::render_plist(launch_agents_dir, fno_binary, install_path, interval)
}
