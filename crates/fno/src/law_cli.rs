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
pub const NATIVE_LAW_SUBCOMMANDS: &[&str] = &["set", "stage", "match", "retract", "history"];

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

/// Classify `fno inbox decide ...` for the front door: the decide record
/// verb runs natively through the same worker lane as the law door,
/// `None` forwards to the Python CLI for the rest of the `inbox` tree.
pub fn classify_inbox_decide(args: &[OsString]) -> Option<Vec<OsString>> {
    if args.len() < 2 {
        return None;
    }
    if args[0].to_str() == Some("inbox") && args[1].to_str() == Some("decide") {
        Some(args[2..].to_vec())
    } else {
        None
    }
}

/// Classify `fno inbox decisions ...` for the front door: the listing read
/// runs natively through the same worker lane, `None` forwards to the
/// Python CLI for the rest of the `inbox` tree.
pub fn classify_inbox_decisions(args: &[OsString]) -> Option<Vec<OsString>> {
    if args.len() < 2 {
        return None;
    }
    if args[0].to_str() == Some("inbox") && args[1].to_str() == Some("decisions") {
        Some(args[1..].to_vec())
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
            "usage: fno inbox law set <subject> [decision] [--global] [--paths g1,g2] [--rationale s] [--option s]... [--supersedes d-x] [--graduation k] [--graduation-ref r] [--decision-file f|-] [--read cmd]... | fno inbox law stage | fno inbox law match (one JSON request on stdin) | fno inbox law retract <subject-or-id> --reason <why> | fno inbox law history <subject-or-id>"
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
            // A piped stdin rides the request ONLY for `--decision-file -`:
            // every other call never touches stdin, so a daemon whose stdin
            // is an open pipe cannot block the verb.
            let stdin_text = if stdin_requested(&argv) {
                let mut buf = String::new();
                let _ = std::io::stdin().read_to_string(&mut buf);
                buf
            } else {
                String::new()
            };
            let request = serde_json::json!({
                "mode": "record",
                "argv": argv,
                "stdin": stdin_text,
            })
            .to_string();
            law_exec(&request, Attended::No)
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
            law_exec(&buf, Attended::No)
        }
        "retract" => {
            let argv: Vec<String> = rest
                .iter()
                .filter_map(|a| a.to_str().map(str::to_owned))
                .collect();
            let request = serde_json::json!({
                "mode": "retract",
                "argv": argv,
            })
            .to_string();
            // Attended: the request rides argv and the worker inherits this
            // terminal as stdin, so the door's operator proof is the real
            // tty - the one credential a piped lane can never carry.
            law_exec(&request, Attended::Yes)
        }
        "history" => {
            let argv: Vec<String> = rest
                .iter()
                .filter_map(|a| a.to_str().map(str::to_owned))
                .collect();
            let request = serde_json::json!({
                "mode": "history",
                "argv": argv,
            })
            .to_string();
            law_exec(&request, Attended::No)
        }
        _ => {
            eprintln!("error: expected a subcommand (set | stage | match | retract | history)");
            2
        }
    }
}

/// The `fno inbox decisions` entry: the listing ride through the worker's
/// one-shot lane, unattended (a read takes no operator proof). Help rides
/// argv too; the door's own usage answers it.
pub fn run_decisions(args: &[OsString]) -> i32 {
    let argv: Vec<String> = args
        .iter()
        .skip(1)
        .filter_map(|a| a.to_str().map(str::to_owned))
        .collect();
    let request = serde_json::json!({"mode": "decisions", "argv": argv}).to_string();
    law_exec(&request, Attended::No)
}

/// Stdin is read only when the argv asks for it: `--decision-file` whose
/// value is `-`. A tty check cannot tell an attended call from a daemon,
/// but the argv can.
fn stdin_requested(argv: &[String]) -> bool {
    // Both spellings the record door parses: two arguments, and the inline
    // `--decision-file=-` form its flag=value splitter accepts.
    argv.iter().any(|arg| {
        arg.split_once('=')
            .is_some_and(|(f, v)| f == "--decision-file" && v == "-")
    }) || argv
        .iter()
        .zip(argv.iter().skip(1))
        .any(|(flag, value)| flag == "--decision-file" && value == "-")
}

/// Whether the child inherits this process's stdin. `Yes` is the attended
/// shape: the request rides argv and the worker's fd 0 is the caller's real
/// terminal, which is the operator proof the retract door reads.
enum Attended {
    Yes,
    No,
}

/// The `fno inbox decide` entry: the record ride through the worker's
/// one-shot law lane. Stdin is INHERITED, not piped: decide reads no
/// stdin itself, and an attended recording needs the real terminal as
/// the worker's fd 0 so the superuser lane can open.
pub fn run_decide(args: &[OsString]) -> i32 {
    let argv: Vec<String> = args
        .iter()
        .filter_map(|a| a.to_str().map(str::to_owned))
        .collect();
    let request = serde_json::json!({"mode": "decide", "argv": argv}).to_string();
    law_exec(&request, Attended::Yes)
}

/// One request through the worker's one-shot law lane. The child owns stdout
/// and the exit code; unattended, the request rides a piped stdin. A missing
/// worker is the door's refused exit (3), never an empty answer.
fn law_exec(request: &str, attended: Attended) -> i32 {
    let Some(binary) = crate::store_client::worker_binary() else {
        eprintln!(
            "fno inbox law: refused: the fno-agents-worker binary is unavailable (set FNO_AGENTS_WORKER, or install the worker beside fno)."
        );
        return 3;
    };
    let mut cmd = Command::new(&binary);
    let piped = match attended {
        Attended::Yes => {
            cmd.arg("--law-exec-arg").arg(request);
            cmd.stdin(Stdio::inherit());
            None
        }
        Attended::No => {
            cmd.arg("--law-exec");
            cmd.stdin(Stdio::piped());
            Some(request)
        }
    };
    let mut child = match cmd
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
    if let Some(request) = piped {
        let sent = match child.stdin.as_mut() {
            Some(stdin) => stdin.write_all(request.as_bytes()).map_err(|_| ()),
            None => Err(()),
        };
        if sent.is_err() {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("fno inbox law: refused: could not send the law request");
            return 3;
        }
        drop(child.stdin.take());
        // stdin dropped here: the child sees EOF and answers.
    }
    match child.wait() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("fno inbox law: refused: {e}");
            3
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_only_for_decision_file_dash() {
        // A daemon whose stdin is an open pipe must never block the
        // verb. Only `--decision-file -` (either spelling) asks for stdin.
        assert!(stdin_requested(&[
            "subject".to_string(),
            "--decision-file".to_string(),
            "-".to_string(),
        ]));
        assert!(stdin_requested(&[
            "topic".to_string(),
            "--decision-file=-".to_string()
        ]));
        assert!(!stdin_requested(&[
            "subject".to_string(),
            "text".to_string()
        ]));
        assert!(!stdin_requested(&[
            "subject".to_string(),
            "--decision-file".to_string(),
            "plan.md".to_string(),
        ]));
        assert!(!stdin_requested(&[
            "topic".to_string(),
            "--decision-file=plan.md".to_string()
        ]));
        assert!(!stdin_requested(&[]));
    }

    #[test]
    fn classify_routes_new_subcommands() {
        let mk = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            classify_inbox_law(&mk(&["inbox", "law", "retract"])).map(|r| r.len()),
            Some(1)
        );
        assert_eq!(
            classify_inbox_law(&mk(&["inbox", "law", "history"])).map(|r| r.len()),
            Some(1)
        );
        // Unknown subcommands still fall through to the Python group.
        assert!(classify_inbox_law(&mk(&["inbox", "law", "nope"])).is_none());
        assert!(classify_inbox_law(&mk(&["inbox", "law"])).is_none());
    }

    #[test]
    fn classify_routes_the_decisions_listing() {
        let mk = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        // The subcommand token itself plus the caller's flags.
        assert_eq!(
            classify_inbox_decisions(&mk(&["inbox", "decisions", "--json"])).map(|r| r.len()),
            Some(2)
        );
        assert!(classify_inbox_decisions(&mk(&["inbox", "decisions"])).is_some());
        // The rest of the inbox tree stays Python's.
        assert!(classify_inbox_decisions(&mk(&["inbox", "law"])).is_none());
        assert!(classify_inbox_decisions(&mk(&["inbox", "decide"])).is_none());
        assert!(classify_inbox_decisions(&mk(&["inbox"])).is_none());
    }
}
