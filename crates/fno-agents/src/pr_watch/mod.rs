//! The `pr-watch` verb family's Rust home. `status` lives here first; the
//! other five verbs (heal, refresh, install, uninstall, tick) follow, each
//! one replacing a Python leaf with a forward until the Python package is
//! gone. One verb per PR, every spelling kept.

mod render;
mod status;

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("status") => status::run(&args[1..]),
        _ => {
            eprintln!("usage: fno-agents pr-watch <status> [--json|-J]");
            2
        }
    }
}
