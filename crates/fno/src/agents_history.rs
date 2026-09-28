//! Read-only session history assembled from the stores that already own it.

use serde_json::Value;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;

mod render;
mod resolve;
mod sources;
mod transcript;

use render::{card, event_sid, event_time, ledger_line};
use resolve::resolve;
use sources::{load_sources, Paths, Receipt, Sources};
use transcript::{find_transcript, scan, TranscriptFacts};

pub fn classify(args: &[OsString]) -> Option<Vec<OsString>> {
    if args.len() < 3
        || args[0].to_str()? != "agents"
        || args[1].to_str()? != "history"
        || !args[2..].iter().any(|arg| arg == "--graph")
    {
        return None;
    }
    Some(args[2..].to_vec())
}

/// Run the native reader and return its process exit code.
pub fn run(args: &[OsString]) -> i32 {
    let stdout = io::stdout();
    let stderr = io::stderr();
    run_to(args, &mut stdout.lock(), &mut stderr.lock())
}

fn run_to(args: &[OsString], stdout: &mut impl Write, stderr: &mut impl Write) -> i32 {
    let paths = match parse_args(args) {
        Ok(paths) => paths,
        Err(message) => {
            let _ = writeln!(stderr, "error: {message}");
            return 2;
        }
    };
    let sources = load_sources(&paths);
    let resolved = resolve(&paths.arg, &sources);
    let mut output = Vec::new();

    if resolved.repo_slug_unresolved {
        let _ = writeln!(
            output,
            "repo slug unresolved; PR numbers collide across repos"
        );
    }
    for sid in &resolved.sessions {
        let transcript = find_transcript(sid);
        for line in card(sid, &sources, transcript.as_ref()) {
            let _ = writeln!(output, "{line}");
        }
        if sid != resolved.sessions.last().unwrap_or(sid) || !resolved.ledger_only.is_empty() {
            let _ = writeln!(output, "---");
        }
    }
    for (index, entry) in resolved.ledger_only.iter().enumerate() {
        if entry["node_id_unrecoverable"].as_bool() == Some(true) {
            let _ = writeln!(
                output,
                "node: not recorded (this row says node_id_unrecoverable)"
            );
        }
        let _ = writeln!(output, "{}", ledger_line(entry));
        let _ = writeln!(
            output,
            "session: not recorded (ledger uuid coverage is write-path only; this row predates it)"
        );
        if index + 1 < resolved.ledger_only.len() {
            let _ = writeln!(output, "---");
        }
    }

    if resolved.sessions.is_empty() && resolved.ledger_only.is_empty() {
        let _ = writeln!(
            stderr,
            "not recorded: no source answers '{}' (registry, reap receipts, graph, ledger and event log consulted; tried harness_session_id, short_id, name, aliases, receipt keys harness_session_id, short_id, row_name, node session rows, ledger session fields and event session fields)",
            paths.arg
        );
        return 1;
    }
    if stdout.write_all(&output).is_err() {
        let _ = writeln!(stderr, "error: could not write session history");
        return 1;
    }
    0
}

fn parse_args(args: &[OsString]) -> Result<Paths, String> {
    let mut arg = None;
    let mut graph = None;
    let mut ledger = None;
    let mut events = None;
    let mut agents_home = None;
    let mut repo_slug = None;
    let mut iter = args.iter();
    while let Some(token) = iter.next() {
        let Some(token) = token.to_str() else {
            return Err("arguments must be valid UTF-8".into());
        };
        match token {
            "--graph" => graph = Some(next_path(&mut iter, "--graph")?),
            "--ledger" => ledger = Some(next_path(&mut iter, "--ledger")?),
            "--events" => events = Some(next_path(&mut iter, "--events")?),
            "--agents-home" => agents_home = Some(next_path(&mut iter, "--agents-home")?),
            "--repo-slug" => {
                repo_slug = Some(
                    iter.next()
                        .ok_or_else(|| "--repo-slug requires a value".to_string())?
                        .to_string_lossy()
                        .into_owned(),
                )
            }
            value if value.starts_with('-') => return Err(format!("unknown option {value}")),
            value if arg.is_none() => arg = Some(value.to_string()),
            value => return Err(format!("unexpected positional argument {value}")),
        }
    }
    Ok(Paths {
        arg: arg.ok_or_else(|| "a session, node or PR argument is required".to_string())?,
        graph: graph.ok_or_else(|| "--graph <path> is required".to_string())?,
        ledger: ledger.ok_or_else(|| "--ledger <path> is required".to_string())?,
        events: events.ok_or_else(|| "--events <path> is required".to_string())?,
        agents_home: agents_home.ok_or_else(|| "--agents-home <dir> is required".to_string())?,
        repo_slug,
    })
}

fn next_path<'a>(
    iter: &mut impl Iterator<Item = &'a OsString>,
    flag: &str,
) -> Result<PathBuf, String> {
    iter.next()
        .map(PathBuf::from)
        .ok_or_else(|| format!("{flag} requires a path"))
}
pub(super) fn short_id(value: &str) -> String {
    value.chars().take(8).collect()
}

pub(super) fn str_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
}

fn string_at<'a>(value: &'a Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| str_at(value, key).map(str::to_string))
}

pub(super) fn eq(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

pub(super) fn push_string_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|old| eq(old, &value)) {
        values.push(value);
    }
}

pub(super) fn push_unique(values: &mut Vec<Value>, value: Value) {
    if !values.contains(&value) {
        values.push(value);
    }
}
#[cfg(test)]
#[path = "agents_history_tests.rs"]
mod tests;
