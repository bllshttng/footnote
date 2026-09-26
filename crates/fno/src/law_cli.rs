//! Native law-door verbs under `fno inbox law`.
//!
//! The Python `fno inbox` group keeps every other name; `law set`, `law
//! stage`, and `law match` run natively. The door's machinery lives in the
//! runtime crate (`fno_agents::law_match`), shared with question_intake's
//! matcher; these verbs are thin stdin-to-library pipes. No top-level verb
//! exists (law d-fe66560a); the group stays nested under `inbox`.

use std::ffi::OsString;
use std::io::Read;

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
            fno_agents::law_match::run_law_match_str(&request)
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
            fno_agents::law_match::run_law_match_str(&buf)
        }
        _ => {
            eprintln!("error: expected a subcommand (set | stage | match)");
            2
        }
    }
}

fn stdin_is_tty() -> bool {
    unsafe { libc::isatty(0) == 1 }
}
