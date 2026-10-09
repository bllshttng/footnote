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
mod tests {
    use super::*;
    use crate::graph_store;

    fn seed(graph: &Path) {
        let rows = json!([
            {"id": "x-aa11", "slug": "a", "title": "A", "type": "feature", "status": "idea",
             "priority": "p2", "domain": "code", "created_at": "2026-10-09T00:00:00+00:00",
             "sessions": [{"phase": "do", "harness": "claude", "session_id": "dfd94113-0000-0000-0000-000000000001"}]},
            {"id": "x-bb22", "slug": "b", "title": "B", "type": "feature", "status": "idea",
             "priority": "p2", "domain": "code", "created_at": "2026-10-09T00:00:00+00:00",
             "spawned_by_session": "born-parent",
             "sessions": [{"phase": "do", "harness": "claude", "session_id": "529383fe-0000-0000-0000-000000000002"}]},
            {"id": "x-cc33", "slug": "c", "title": "C", "type": "feature", "status": "idea",
             "priority": "p2", "domain": "code", "created_at": "2026-10-09T00:00:00+00:00",
             "source_session_id": "707aab13-0000-0000-0000-00000000000a", "source_harness": "claude",
             "sessions": [{"phase": "do", "harness": "claude", "session_id": "1e4b7675-0000-0000-0000-000000000003"}]},
        ]);
        graph_store::seed_rows(graph, rows.as_array().unwrap()).unwrap();
    }

    const MAP: &str = "\
| launched (UTC) | lead session | worker name | worker session |
|---|---|---|---|
| 2026-10-08T20:46:40Z | 707aab13 | t-x-bb22-opus | 529383fe |
| 2026-10-08T20:23:10Z | 707aab13 | t-x-aa11-sonnet | dfd94113 |
| 2026-10-08T21:00:00Z | 0000dead | t-gone | 1e4b7675 |
| 2026-10-08T21:10:00Z | 707aab13 | t-none | c922c004 |
";

    #[test]
    fn a_dry_run_reports_every_row_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        seed(&graph);
        let map = dir.path().join("map.md");
        std::fs::write(&map, MAP).unwrap();
        let journal = dir.path().join("backfill.jsonl");
        let lines = backfill(&graph, &map, false, &journal).unwrap();
        assert!(
            lines[0].starts_with("would-write\tt-x-aa11-sonnet"),
            "{lines:?}"
        );
        assert!(lines[1].starts_with("kept\tt-x-bb22-opus") && lines[1].contains("born-parent"));
        assert!(lines[2].starts_with("skip\tt-gone") && lines[2].contains("none"));
        assert!(lines[3].starts_with("skip\tt-none") && lines[3].contains("no single node"));
        let back = graph_store::read_rows(&graph).unwrap();
        assert!(back.iter().find(|r| r["id"] == "x-aa11").unwrap()["spawned_by_session"].is_null());
        assert!(!journal.exists());
    }

    #[test]
    fn apply_writes_empty_edges_once_and_journals_each_write() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        seed(&graph);
        let map = dir.path().join("map.md");
        std::fs::write(&map, MAP).unwrap();
        let journal = dir.path().join("backfill.jsonl");
        backfill(&graph, &map, true, &journal).unwrap();
        let back = graph_store::read_rows(&graph).unwrap();
        let row = |id: &str| back.iter().find(|r| r["id"] == id).unwrap().clone();
        assert_eq!(
            row("x-aa11")["spawned_by_session"],
            "707aab13-0000-0000-0000-00000000000a"
        );
        assert_eq!(row("x-bb22")["spawned_by_session"], "born-parent");
        assert_eq!(
            std::fs::read_to_string(&journal).unwrap().lines().count(),
            1
        );
        // A second apply finds the edge and keeps it.
        let again = backfill(&graph, &map, true, &journal).unwrap();
        assert!(again[0].starts_with("kept\tt-x-aa11-sonnet"), "{again:?}");
        assert_eq!(
            std::fs::read_to_string(&journal).unwrap().lines().count(),
            1
        );
    }

    #[test]
    fn a_lead_prefix_with_two_sessions_is_skipped_as_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        let rows = json!([
            {"id": "x-aa11", "slug": "a", "title": "A", "type": "feature", "status": "idea",
             "priority": "p2", "domain": "code", "created_at": "2026-10-09T00:00:00+00:00",
             "source_session_id": "abcd0001-0000-0000-0000-000000000000",
             "sessions": [{"phase": "do", "harness": "claude", "session_id": "dfd94113-0000-0000-0000-000000000001"}]},
            {"id": "x-bb22", "slug": "b", "title": "B", "type": "feature", "status": "idea",
             "priority": "p2", "domain": "code", "created_at": "2026-10-09T00:00:00+00:00",
             "source_session_id": "abcd0002-0000-0000-0000-000000000000"},
        ]);
        graph_store::seed_rows(&graph, rows.as_array().unwrap()).unwrap();
        let map = dir.path().join("map.md");
        std::fs::write(&map, "| 2026-10-08T20:23:10Z | abcd | w | dfd94113 |\n").unwrap();
        let lines = backfill(&graph, &map, true, &dir.path().join("j")).unwrap();
        assert!(
            lines[0].starts_with("skip\tw") && lines[0].contains("ambiguous (2 matches)"),
            "{lines:?}"
        );
        let back = graph_store::read_rows(&graph).unwrap();
        assert!(back.iter().find(|r| r["id"] == "x-aa11").unwrap()["spawned_by_session"].is_null());
    }
}
