//! Native law-door verbs under `fno inbox law`.
//!
//! The Python `fno inbox` group keeps every other name; `law set`, `law
//! stage`, and `law match` run natively. The door's machinery stays in the
//! runtime crate, reached through the worker's one-shot `--law-exec` lane:
//! the mux never links the runtime (crates/fno/tests/product_boundary.rs),
//! so these verbs spawn the worker exactly the way the store's `--store-exec`
//! clients do. No top-level verb exists (law d-fe66560a); the group stays
//! nested under `inbox`.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::process::{Command, Stdio};

/// The verbs the native surface serves. The Python `fno inbox law` group
/// forwards here instead of holding its own write path.
pub const NATIVE_LAW_SUBCOMMANDS: &[&str] = &["set", "stage", "match"];

/// Classify `fno inbox law <sub> ...` for the front door: `Some(rest)` runs
/// natively, `None` forwards to the Python CLI.
pub fn classify_inbox_law(args: &[OsString]) -> Option<Vec<OsString>> {
    if args.len() < 3 {
        return None;
    }
    let a0 = args[0].to_str()?;
    let a1 = args[1].to_str()?;
    let a2 = args[2].to_str()?;
    if a0 == "inbox" && a1 == "law" && NATIVE_LAW_SUBCOMMANDS.contains(&a2) {
        Some(args[2..].to_vec())
    } else {
        None
    }
}

/// Parse and run one native law subcommand; returns the exit code. `set`
/// carries the door's exit contract (0 recorded, 1 recorded-but-index-failed,
/// 3 refused) and prints the decision id alone; `stage` and `match` print one
/// JSON answer and exit 0.
pub fn run(args: &[OsString]) -> i32 {
    let sub = args.first().and_then(|a| a.to_str()).unwrap_or("");
    if args
        .iter()
        .any(|a| a.to_str() == Some("-h") || a.to_str() == Some("--help"))
    {
        println!(
            "usage: fno inbox law set <subject> [decision] [--global] [--paths g1,g2] [--rationale s] [--option s]... [--supersedes d-x] [--graduation k] [--graduation-ref r] [--decision-file f|-] [--read cmd]... | fno inbox law stage | fno inbox law match (one JSON request on stdin)"
        );
        return 0;
    }
    let rest: &[OsString] = if args.is_empty() { &[] } else { &args[1..] };
    match sub {
        "set" => {
            let argv: Vec<String> = rest
                .iter()
                .filter_map(|a| a.to_str().map(str::to_owned))
                .collect();
            // A piped stdin rides the request for `--decision-file -`; a tty
            // reads nothing, so an interactive call never blocks.
            let stdin_text = if stdin_is_tty() {
                String::new()
            } else {
                let mut buf = String::new();
                let _ = std::io::stdin().read_to_string(&mut buf);
                buf
            };
            let request = serde_json::json!({
                "mode": "record",
                "argv": argv,
                "stdin": stdin_text,
            })
            .to_string();
            law_exec(&request)
        }
        "stage" | "match" => {
            // The caller shapes the request (the hook wraps its payload as a
            // stage request; the Python transport sends any other mode); it
            // runs verbatim.
            let mut buf = String::new();
            if std::io::stdin().read_to_string(&mut buf).is_err() {
                eprintln!("fno inbox law {sub}: could not read the request from stdin");
                return 2;
            }
            law_exec(&buf)
        }
        _ => {
            eprintln!("error: expected a subcommand (set | stage | match)");
            2
        }
    }
}

/// One request through the worker's `--law-exec` one-shot lane. The child
/// owns stdout and the exit code; the request rides stdin. A missing worker
/// is the door's refused exit (3), never an empty answer.
fn law_exec(request: &str) -> i32 {
    let Some(binary) = crate::store_client::worker_binary() else {
        eprintln!(
            "fno inbox law: refused: the fno-agents-worker binary is unavailable (set FNO_AGENTS_WORKER, or install the worker beside fno)."
        );
        return 3;
    };
    let mut child = match Command::new(&binary)
        .arg("--law-exec")
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("fno inbox law: refused: cannot spawn the law worker: {e}");
            return 3;
        }
    };
    let sent = child
        .stdin
        .take()
        .ok_or(())
        .and_then(|mut stdin| stdin.write_all(request.as_bytes()).map_err(|_| ()));
    if sent.is_err() {
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("fno inbox law: refused: could not send the law request");
        return 3;
    }
    // stdin dropped here: the child sees EOF and answers.
    match child.wait() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("fno inbox law: refused: {e}");
            3
        }
    }
}

fn stdin_is_tty() -> bool {
    unsafe { libc::isatty(0) == 1 }
}
