//! The two writers on `fno backlog provenance`: the node's first-launch
//! stamp (`--stamp-launch`, which the Python spawn front forwards to) and the
//! receipted backfill of launches nobody recorded (`--backfill <map.md>`).
//! Every other shape of the command stays the Python read.
//!
//! Both write only an empty `spawned_by_session`: launch is the FIRST launch,
//! and a node's existing edge is never overwritten.

use crate::launch_record::{stamp_node, Stamp};
use rusqlite::OptionalExtension;
use serde_json::json;
use std::collections::HashSet;
use std::path::Path;

/// Whether this tail is one of the two native shapes.
pub fn owns(tail: &[String]) -> bool {
    tail.iter()
        .any(|a| a == "--stamp-launch" || a == "--backfill" || a.starts_with("--backfill="))
}

pub fn run(tail: &[String]) -> i32 {
    if super::workflows::active_backend_name() != "graph" {
        eprintln!("fno backlog provenance: the launch edge lives in the graph store; refused under another tracker backend.");
        return 1;
    }
    let graph = super::settings::graph_path();
    if tail.iter().any(|a| a == "--stamp-launch") {
        return run_stamp_launch(&graph, tail);
    }
    let map = tail.iter().enumerate().find_map(|(i, a)| {
        a.strip_prefix("--backfill=")
            .map(str::to_string)
            .or_else(|| {
                (a == "--backfill")
                    .then(|| tail.get(i + 1).cloned())
                    .flatten()
            })
    });
    let Some(map) = map else {
        eprintln!("usage: fno backlog provenance --backfill <map.md> [--apply]");
        return 2;
    };
    let apply = tail.iter().any(|a| a == "--apply");
    let journal = crate::paths::AgentsHome::from_env()
        .root()
        .join("launches")
        .join("backfill.jsonl");
    match backfill(&graph, Path::new(&map), apply, &journal) {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
            0
        }
        Err(e) => {
            eprintln!("fno backlog provenance --backfill: {e}");
            1
        }
    }
}

fn run_stamp_launch(graph: &Path, tail: &[String]) -> i32 {
    // `--session/--harness/--cwd` carry a parent edge the caller already
    // proved (the Python spawn front); otherwise this process reads its own.
    let mut given: [Option<String>; 3] = [None, None, None];
    let mut node = None;
    let mut it = tail.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--session" => given[0] = it.next().cloned(),
            "--harness" => given[1] = it.next().cloned(),
            "--cwd" => given[2] = it.next().cloned(),
            a if !a.starts_with('-') && node.is_none() => node = Some(a.to_string()),
            _ => {}
        }
    }
    let Some(node) = node else {
        eprintln!("usage: fno backlog provenance <node> --stamp-launch [--session <id> --harness <h> --cwd <dir>]");
        return 2;
    };
    let [given_session, given_harness, given_cwd] = given;
    let (session, harness, cwd) = match given_session {
        Some(s) => (Some(s), given_harness, given_cwd),
        None => crate::claims::ambient_parent_edge(),
    };
    let Some(session) = session else {
        // No proven parent session, no write: a null session on a durable
        // node asserts a launch nobody can trace.
        eprintln!(
            "spawn: launch edge not recorded on {node}: no parent session in this environment."
        );
        return 1;
    };
    match stamp_node(graph, &node, &session, harness.as_deref(), cwd.as_deref()) {
        Ok(Stamp::Wrote) => 0,
        Ok(Stamp::Kept(who)) => {
            eprintln!("spawn: launch edge on {node} already names {who}; kept.");
            0
        }
        Ok(Stamp::NoNode) => {
            eprintln!("spawn: launch edge not recorded on {node} (node not in graph); the edge was not written.");
            1
        }
        Err(e) => {
            eprintln!("spawn: launch edge not recorded on {node}: {e}");
            1
        }
    }
}

/// One map row: when, which lead, which worker name, which worker sessions.
#[derive(Debug, Clone, PartialEq)]
struct MapRow {
    launched: String,
    lead: String,
    worker: String,
    sessions: Vec<String>,
}

/// The markdown table rows whose first cell is a timestamp. The header and
/// separator rows, and any prose, are skipped.
fn parse_map(text: &str) -> Vec<MapRow> {
    let mut rows: Vec<MapRow> = text
        .lines()
        .filter_map(|line| {
            let cells: Vec<&str> = line
                .trim()
                .strip_prefix('|')?
                .split('|')
                .map(str::trim)
                .collect();
            let launched = *cells.first()?;
            if cells.len() < 4 || !launched.starts_with(|c: char| c.is_ascii_digit()) {
                return None;
            }
            Some(MapRow {
                launched: launched.to_string(),
                lead: cells[1].to_string(),
                worker: cells[2].to_string(),
                sessions: cells[3]
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect(),
            })
        })
        .collect();
    // First launch wins, so the earliest row must be written first.
    rows.sort_by(|a, b| a.launched.cmp(&b.launched));
    rows
}

fn is_hex_prefix(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// The one id a prefix names, or why not.
fn one_match(
    db: &rusqlite::Connection,
    sql: &str,
    prefix: &str,
) -> Result<Result<String, String>, String> {
    if !is_hex_prefix(prefix) {
        return Ok(Err(format!("{prefix:?} is not a session id prefix")));
    }
    let mut stmt = db.prepare(sql).map_err(|e| e.to_string())?;
    let found: Vec<String> = stmt
        .query_map([format!("{prefix}%")], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .collect();
    Ok(match found.len() {
        1 => Ok(found.into_iter().next().unwrap()),
        0 => Err("none".into()),
        n => Err(format!("ambiguous ({n} matches)")),
    })
}

fn backfill(graph: &Path, map: &Path, apply: bool, journal: &Path) -> Result<Vec<String>, String> {
    let text =
        std::fs::read_to_string(map).map_err(|e| format!("cannot read {}: {e}", map.display()))?;
    let rows = parse_map(&text);
    if rows.is_empty() {
        return Err(format!(
            "{} holds no table row whose first cell is a timestamp",
            map.display()
        ));
    }
    let db = super::open(graph)?;
    let mut out = Vec::new();
    // Nodes an earlier row of this run already wrote (or would write).
    let mut taken: HashSet<String> = HashSet::new();
    for row in rows {
        let lead = match one_match(
            &db,
            "SELECT id FROM agent_sessions WHERE id LIKE ?1",
            &row.lead,
        )? {
            Ok(id) => id,
            Err(why) => {
                out.push(format!(
                    "skip\t{}\t-\t-\tlead {}: {why}",
                    row.worker, row.lead
                ));
                continue;
            }
        };
        let lead_harness: Option<String> = db
            .query_row(
                "SELECT harness_id FROM agent_sessions WHERE id = ?1",
                [&lead],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?
            .flatten();
        for worker in &row.sessions {
            let node = match one_match(
                &db,
                "SELECT DISTINCT node_id FROM sessions WHERE session_id LIKE ?1",
                worker,
            )? {
                Ok(node) => node,
                Err(why) => {
                    out.push(format!(
                        "skip\t{}\t{worker}\t-\tworker worked no single node: {why}",
                        row.worker
                    ));
                    continue;
                }
            };
            let existing: Option<String> = db
                .query_row(
                    "SELECT spawned_by_session FROM node_provenance WHERE node_id = ?1",
                    [&node],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| e.to_string())?
                .flatten()
                .filter(|s: &String| !s.is_empty());
            if let Some(who) = existing {
                out.push(format!(
                    "kept\t{}\t{worker}\t{node}\talready names {who}",
                    row.worker
                ));
                continue;
            }
            if !taken.insert(node.clone()) {
                out.push(format!(
                    "kept\t{}\t{worker}\t{node}\tan earlier launch in this map wins",
                    row.worker
                ));
                continue;
            }
            if !apply {
                out.push(format!(
                    "would-write\t{}\t{worker}\t{node}\tlead {lead}",
                    row.worker
                ));
                continue;
            }
            match stamp_node(graph, &node, &lead, lead_harness.as_deref(), None)? {
                Stamp::Wrote => {
                    append_journal(
                        journal,
                        &json!({
                            "node": node, "spawned_by_session": lead,
                            "spawned_by_harness": lead_harness, "worker": row.worker,
                            "worker_session": worker, "launched": row.launched,
                            "source": map.display().to_string(),
                            "written_at_ms": crate::claims::now_ms(),
                        }),
                    )?;
                    out.push(format!(
                        "wrote\t{}\t{worker}\t{node}\tlead {lead}",
                        row.worker
                    ));
                }
                Stamp::Kept(who) => out.push(format!(
                    "kept\t{}\t{worker}\t{node}\talready names {who}",
                    row.worker
                )),
                Stamp::NoNode => out.push(format!(
                    "skip\t{}\t{worker}\t{node}\tnode not in graph",
                    row.worker
                )),
            }
        }
    }
    Ok(out)
}

fn append_journal(path: &Path, row: &serde_json::Value) -> Result<(), String> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    writeln!(f, "{row}").map_err(|e| e.to_string())
}

#[cfg(test)]
#[path = "provenance_cli_tests.rs"]
mod tests;
