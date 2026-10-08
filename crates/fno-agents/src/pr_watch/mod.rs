//! The `pr-watch` verb family's Rust home. `status` and `install` live here
//! first; the other four verbs (heal, refresh, uninstall, tick) follow, each
//! one replacing a Python leaf with a working forward until the Python
//! package is gone. One verb per PR, every spelling kept.

mod install;
mod render;
mod status;

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("status") => status::run(&args[1..]),
        Some("install") => install::run(&args[1..]),
        _ => {
            eprintln!("usage: fno-agents pr-watch <status|install> [--json|-J] [-N]");
            2
        }
    }
}
