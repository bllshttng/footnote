//! Whether a ``spawn --resume`` revives an existing row instead of forking:
//! the row is found by name first, then by the resumed uuid itself, and the
//! candidate's supervisor must not be live. Split from the Python spawn
//! dispatch under the file budget. The Python writer-claim gate stays the
//! fail-closed backstop on every revival, so a probe that cannot run reads
//! not-live and the claim decides.

use crate::paths::AgentsHome;
use serde_json::{json, Value};

pub fn run_revival_check(args: &[String], home: &AgentsHome) -> i32 {
    let mut name: Option<String> = None;
    let mut harness: String = "claude".into();
    let mut resume: Option<String> = None;
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--name" => name = iter.next().cloned(),
            "--harness" => harness = iter.next().cloned().unwrap_or_default(),
            "--resume" => resume = iter.next().cloned(),
            _ => {}
        }
    }
    let Some((_, sid)) = resume
        .as_deref()
        .map(|s| (s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .map(|s| (s.clone(), s))
    else {
        eprintln!("revival-check: --resume is required");
        return 2;
    };
    let Some(name) = name.as_deref().map(str::trim).filter(|n| !n.is_empty()) else {
        eprintln!("revival-check: --name is required");
        return 2;
    };
    let rows = match crate::client_verbs::read_registry_entries(&home.registry_json()) {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("revival-check: registry unreadable: {e}");
            return 13;
        }
    };
    let by_name = rows
        .iter()
        .find(|r| r.get("name").and_then(Value::as_str) == Some(name));
    let candidate = by_name.clone().or_else(|| {
        rows.iter()
            .find(|r| r.get("harness_session_id").and_then(Value::as_str) == Some(sid.as_str()))
    });
    let Some(entry) = candidate else {
        println!("{}", json!({ "revive": false, "by": Value::Null }));
        return 0;
    };
    let by = if by_name.is_some() { "name" } else { "session" };
    let revive = harness == "claude"
        && entry.get("harness").and_then(Value::as_str) == Some("claude")
        && entry.get("harness_session_id").and_then(Value::as_str) == Some(sid.as_str())
        && !probe_live(entry.get("messaging_socket_path").and_then(Value::as_str));
    println!(
        "{}",
        json!({ "revive": revive, "by": by, "name": entry.get("name").and_then(Value::as_str).unwrap_or("") })
    );
    0
}

/// The Python probe's fail-safe shape: a row with no recorded socket cannot
/// prove its supervisor live, so it reads not-live and the writer-claim
/// decides. A recorded socket answers through the daemon's probe.
fn probe_live(socket: Option<&str>) -> bool {
    match socket {
        Some(sock) => crate::claude_ask::liveness_probe(sock),
        None => false,
    }
}
