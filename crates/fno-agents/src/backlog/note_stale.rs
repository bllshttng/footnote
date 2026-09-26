//! `backlog-notes stale`: is a plan older than its node's notes?
//!
//! A plan's written-at is its last git commit when one exists, else its file
//! mtime. The basis is always named: an mtime moves on any save, so only a
//! commit time says which text the notes were written against. Advisory like
//! a stale review verdict - the read reports, it never blocks.
use crate::backlog::{node_state, note_history};
use crate::graph_store;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;

fn parse_ts(raw: &str) -> Option<DateTime<Utc>> {
    if let Ok(t) = DateTime::parse_from_rfc3339(raw.trim()) {
        return Some(t.with_timezone(&Utc));
    }
    NaiveDateTime::parse_from_str(raw.trim(), "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .map(|t| t.and_utc())
}

/// The plan's written-at and its basis (`commit` or `mtime`).
fn plan_written_at(plan: &Path) -> Option<(DateTime<Utc>, &'static str)> {
    let dir = plan.parent().filter(|d| !d.as_os_str().is_empty())?;
    let committed = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["log", "-1", "--format=%cI", "--"])
        .arg(plan.file_name()?)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| parse_ts(&String::from_utf8_lossy(&o.stdout)));
    if let Some(t) = committed {
        return Some((t, "commit"));
    }
    let modified = std::fs::metadata(plan).ok()?.modified().ok()?;
    Some((DateTime::<Utc>::from(modified), "mtime"))
}

/// Every note time the node carries: legacy notes, the current state, and
/// each journal record's original.
fn note_times(graph: &Path, node_id: &str, row: &Value) -> Vec<DateTime<Utc>> {
    let stamp = |v: &Value| -> Option<DateTime<Utc>> {
        ["ts", "updated_at", "created_at"]
            .iter()
            .find_map(|k| v.get(*k).and_then(Value::as_str))
            .and_then(parse_ts)
    };
    let mut out: Vec<DateTime<Utc>> = row
        .get("progress_notes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(stamp)
        .collect();
    if let Some(t) = node_state::read_state(row)
        .and_then(|s| s.updated_at)
        .and_then(|u| parse_ts(&u))
    {
        out.push(t);
    }
    if note_history::history_path(graph).exists() {
        if let Ok((records, _)) = note_history::read(graph, Some(node_id), 0, usize::MAX) {
            out.extend(
                records
                    .iter()
                    .filter_map(|r| r.get("original").and_then(stamp)),
            );
        }
    }
    out
}

/// The verdict object for one node and plan. `Err` is a usage failure (exit 2).
pub fn stale_report(graph: &Path, token: &str, plan: &Path) -> Result<Value, String> {
    if !plan.is_file() {
        return Err(format!("plan not found: {}", plan.display()));
    }
    let entries = graph_store::read_rows(graph).unwrap_or_default();
    let Some(row) = crate::graph_get::find_entry(&entries, token) else {
        return Ok(json!({
            "node": token, "plan": plan.display().to_string(),
            "stale": null, "reason": "node not found in graph",
        }));
    };
    let node_id = graph_store::entry_id(row).unwrap_or(token);
    let Some((written, basis)) = plan_written_at(plan) else {
        return Ok(json!({
            "node": node_id, "plan": plan.display().to_string(),
            "stale": null, "reason": "plan written-at unreadable",
        }));
    };
    let newer: Vec<_> = note_times(graph, node_id, row)
        .into_iter()
        .filter(|t| *t > written)
        .collect();
    let newest = newer.iter().max().map(|t| t.to_rfc3339());
    Ok(json!({
        "node": node_id,
        "plan": plan.display().to_string(),
        "stale": !newer.is_empty(),
        "basis": basis,
        "plan_written_at": written.to_rfc3339(),
        "newer_notes": newer.len(),
        "newest_note_at": newest,
    }))
}

/// Dispatch body for `backlog-notes stale`.
pub fn run_stale(graph: &Path, node: Option<&str>, plan: Option<&Path>, json_out: bool) -> i32 {
    let (Some(node), Some(plan)) = (node, plan) else {
        eprintln!("fno-agents backlog-notes stale: needs <node> and --plan <path>");
        return 2;
    };
    if crate::graph_get::external_backend_selected() {
        eprintln!(
            "fno-agents backlog-notes stale: the graph store is not the authoritative \
             backend under the selected external tracker"
        );
        return 2;
    }
    match stale_report(graph, node, plan) {
        Ok(report) => {
            if json_out {
                println!("{report}");
            } else {
                match report.get("stale").and_then(Value::as_bool) {
                    Some(true) => println!(
                        "stale: plan predates {} note(s) on {} (newest {}, basis {})",
                        report["newer_notes"],
                        report["node"].as_str().unwrap_or(node),
                        report["newest_note_at"].as_str().unwrap_or("?"),
                        report["basis"].as_str().unwrap_or("?"),
                    ),
                    Some(false) => println!(
                        "fresh: no note newer than the plan (basis {})",
                        report["basis"].as_str().unwrap_or("?")
                    ),
                    None => println!(
                        "unknown: {}",
                        report["reason"].as_str().unwrap_or("no verdict")
                    ),
                }
            }
            0
        }
        Err(e) => {
            eprintln!("fno-agents backlog-notes stale: {e}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str], env_date: Option<&str>) {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(dir).args(args);
        if let Some(d) = env_date {
            cmd.env("GIT_COMMITTER_DATE", d).env("GIT_AUTHOR_DATE", d);
        }
        let out = cmd.output().expect("git");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn committed_plan(dir: &Path, date: &str) -> std::path::PathBuf {
        git(dir, &["init", "-q"], None);
        git(dir, &["config", "user.email", "t@example.com"], None);
        git(dir, &["config", "user.name", "t"], None);
        let plan = dir.join("plan.md");
        std::fs::write(&plan, "# plan\n").unwrap();
        git(dir, &["add", "plan.md"], None);
        git(dir, &["commit", "-q", "-m", "plan"], Some(date));
        plan
    }

    fn graph_with(dir: &Path, row: Value) -> std::path::PathBuf {
        let graph = dir.join("graph.json");
        crate::graph_store::seed_rows(&graph, &[row]).unwrap();
        graph
    }

    #[test]
    fn a_note_newer_than_the_commit_is_stale_on_the_commit_basis() {
        let repo = tempfile::tempdir().unwrap();
        let plan = committed_plan(repo.path(), "2026-09-01T00:00:00Z");
        let store = tempfile::tempdir().unwrap();
        let graph = graph_with(
            store.path(),
            json!({"id": "x-1", "progress_notes": [
                {"ts": "2026-08-01T00:00:00Z", "note": "old"},
                {"ts": "2026-09-02T00:00:00Z", "note": "new"},
            ]}),
        );
        let r = stale_report(&graph, "x-1", &plan).unwrap();
        assert_eq!(r["stale"], true);
        assert_eq!(r["basis"], "commit");
        assert_eq!(r["newer_notes"], 1);
    }

    #[test]
    fn a_newer_current_state_counts_as_a_note() {
        let repo = tempfile::tempdir().unwrap();
        let plan = committed_plan(repo.path(), "2026-09-01T00:00:00Z");
        let store = tempfile::tempdir().unwrap();
        let graph = graph_with(
            store.path(),
            json!({"id": "x-1", "current_state": {
                "body": "b", "revision": 1, "updated_at": "2026-09-03T00:00:00+00:00"}}),
        );
        let r = stale_report(&graph, "x-1", &plan).unwrap();
        assert_eq!(r["stale"], true);
        assert_eq!(r["newer_notes"], 1);
    }

    #[test]
    fn an_uncommitted_plan_newer_than_every_note_is_fresh_on_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("plan.md");
        std::fs::write(&plan, "# plan\n").unwrap();
        let graph = graph_with(
            dir.path(),
            json!({"id": "x-1", "progress_notes": [{"ts": "2020-01-01T00:00:00Z"}]}),
        );
        let r = stale_report(&graph, "x-1", &plan).unwrap();
        assert_eq!(r["stale"], false);
        assert_eq!(r["basis"], "mtime");
    }

    #[test]
    fn an_unknown_node_reports_null_and_exits_zero() {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("plan.md");
        std::fs::write(&plan, "# plan\n").unwrap();
        let graph = graph_with(dir.path(), json!({"id": "x-1"}));
        let r = stale_report(&graph, "x-nope", &plan).unwrap();
        assert!(r["stale"].is_null());
        assert!(r["reason"].is_string());
        assert_eq!(run_stale(&graph, Some("x-nope"), Some(&plan), true), 0);
        assert_eq!(
            run_stale(&graph, Some("x-1"), Some(&dir.path().join("gone.md")), true),
            2
        );
    }
}
