//! When a session row's phase ends without its own writer saying so. A ship
//! row ends when its PR merges, at the merge instant, or when the PR closes
//! unmerged, at its close. A think, blueprint or review row ends when its
//! session retires, at the transcript's last event. A do row keeps its own
//! settle in gc_sweep, which gates on additional PRs. No time is invented:
//! a row whose end has no recorded source stays open.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::backlog::api::{self, Store};
use crate::graph_store::entry_id;
use crate::paths::AgentsHome;

/// Phases whose row ends with its session: do has its own settle, ship
/// ends with its PR.
const SESSION_BOUND_PHASES: [&str; 3] = ["think", "blueprint", "review"];

/// The merge path's stamp: each (node, merge instant) pair ends that node's
/// open ship rows. Returns (node, reason) for every close that did not land.
pub(crate) fn close_ship_rows_at(
    store: &Store,
    ends: &[(String, String)],
    ended_by: &str,
) -> Vec<(String, String)> {
    if ends.is_empty() {
        return Vec::new();
    }
    let Ok(entries) = api::rows(store) else {
        return vec![("*".into(), "graph unreadable".into())];
    };
    let open: HashSet<&str> = entries
        .iter()
        .filter(|entry| open_ship(entry))
        .filter_map(entry_id)
        .collect();
    let plan: Vec<(String, String, &str)> = ends
        .iter()
        .filter(|(node, _)| open.contains(node.as_str()))
        .map(|(node, at)| (node.clone(), at.clone(), ended_by))
        .collect();
    end_ship(store, &plan)
}

/// The sweep's ship pass, which is also the backfill: every open ship row on
/// a node whose PR merged ends at the merge commit's time on its repo's
/// origin/main. A PR closed unmerged ends at GitHub's closed_at. A merge
/// not yet on the local origin/main waits for a later pass.
pub(crate) fn settle_ship_rows(home: &AgentsHome) -> Vec<(String, String)> {
    let store = Store::new(&crate::gc_sweep::graph_path(home));
    let Ok(entries) = api::rows(&store) else {
        return vec![("*".into(), "graph unreadable".into())];
    };
    end_ship(&store, &ship_backfill_plan(&entries))
}

/// (node, ended_at, ended_by) for each open ship row a recorded merge or
/// close can end, looked up in git and GitHub.
pub(crate) fn ship_backfill_plan(entries: &[Value]) -> Vec<(String, String, &'static str)> {
    let mut merges: HashMap<String, Option<HashMap<u64, String>>> = HashMap::new();
    plan_ship_ends(entries, &mut |cwd, pr, merged| {
        if merged {
            merges
                .entry(cwd.to_string())
                .or_insert_with(|| merge_commit_times(cwd))
                .as_ref()?
                .get(&pr)
                .cloned()
        } else {
            gh_closed_at(pr, cwd)
        }
    })
}

/// Close the think, blueprint and review rows of sessions that just retired,
/// each at its transcript's last event, or now when that cannot be read.
pub(crate) fn close_retired_rows<'a>(
    home: &AgentsHome,
    retired: impl Iterator<Item = &'a crate::state::RegistryEntry>,
) -> Vec<(String, String)> {
    let sessions: HashSet<String> = retired
        .filter_map(|entry| entry.harness_session_id.as_deref())
        .map(|sid| sid.trim().to_ascii_lowercase())
        .filter(|sid| !sid.is_empty())
        .collect();
    if sessions.is_empty() {
        return Vec::new();
    }
    close_session_rows(&Store::new(&crate::gc_sweep::graph_path(home)), &sessions)
}

fn close_session_rows(store: &Store, sessions: &HashSet<String>) -> Vec<(String, String)> {
    let Ok(entries) = api::rows(store) else {
        return vec![("*".into(), "graph unreadable".into())];
    };
    let mut refused = Vec::new();
    for (node, harness, session_id, phase) in plan_retired_closes(&entries, sessions) {
        let tail = crate::claude_adopt::transcript_stamp(&session_id);
        if let Err(err) = api::session_end(
            store,
            &node,
            &session_id,
            "reap-sweep",
            Some(phase),
            Some(&harness),
            tail.as_deref(),
        ) {
            refused.push((node, format!("{phase} row: {}", err.0)));
        }
    }
    refused
}

fn end_ship(store: &Store, plan: &[(String, String, &str)]) -> Vec<(String, String)> {
    match api::phase_end(store, "ship", plan) {
        Ok(_) => Vec::new(),
        Err(err) => vec![("*".into(), format!("ship rows: {}", err.0))],
    }
}

fn sessions_of(entry: &Value) -> impl Iterator<Item = &Value> {
    entry
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn open_ship(entry: &Value) -> bool {
    sessions_of(entry).any(|row| {
        row.get("phase").and_then(Value::as_str) == Some("ship")
            && row
                .get("ended_at")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
    })
}

/// (node, ended_at, ended_by) for each node holding an open ship row whose
/// PR is recorded merged or closed, where `end_of(cwd, pr, merged)` finds
/// the recorded instant.
fn plan_ship_ends(
    entries: &[Value],
    end_of: &mut dyn FnMut(&str, u64, bool) -> Option<String>,
) -> Vec<(String, String, &'static str)> {
    let mut plan = Vec::new();
    for entry in entries.iter().filter(|entry| open_ship(entry)) {
        let (Some(id), Some(pr), Some(cwd)) = (
            entry_id(entry),
            entry.get("pr_number").and_then(Value::as_u64),
            entry.get("cwd").and_then(Value::as_str),
        ) else {
            continue;
        };
        let merged = match entry.get("merge_status").and_then(Value::as_str) {
            Some("merged") => true,
            Some("closed") => false,
            _ => continue,
        };
        if let Some(at) = end_of(cwd, pr, merged) {
            let by = if merged { "merge-commit" } else { "pr-closed" };
            plan.push((id.to_string(), at, by));
        }
    }
    plan
}

fn plan_retired_closes(
    entries: &[Value],
    sessions: &HashSet<String>,
) -> Vec<(String, String, String, &'static str)> {
    let mut plan = Vec::new();
    for entry in entries {
        let Some(id) = entry_id(entry) else {
            continue;
        };
        for row in sessions_of(entry) {
            let Some(phase) = SESSION_BOUND_PHASES
                .into_iter()
                .find(|phase| crate::graph_store::is_open_phase_row(row, phase))
            else {
                continue;
            };
            let field = |key: &str| row.get(key).and_then(Value::as_str).unwrap_or_default();
            if sessions.contains(&field("session_id").trim().to_ascii_lowercase()) {
                plan.push((
                    id.to_string(),
                    field("harness").to_string(),
                    field("session_id").to_string(),
                    phase,
                ));
            }
        }
    }
    plan
}

fn merge_commit_times(cwd: &str) -> Option<HashMap<u64, String>> {
    let out = crate::loopcheck::bounded_read(
        "git".as_ref(),
        &["log", "origin/main", "--first-parent", "--format=%cI%x09%s"],
        Path::new(cwd),
        "gc-sweep",
        Duration::from_secs(30),
    )
    .ok()?;
    out.status
        .success()
        .then(|| parse_merge_commits(&String::from_utf8_lossy(&out.stdout)))
}

/// PR number to UTC merge instant, from `%cI<TAB>%s` lines: a `Merge pull
/// request #N from` subject, or a squash subject ending `(#N)`. The log runs
/// newest first, and the newest line wins a repeat.
fn parse_merge_commits(log: &str) -> HashMap<u64, String> {
    let mut out = HashMap::new();
    for line in log.lines() {
        let Some((at, subject)) = line.split_once('\t') else {
            continue;
        };
        let number = subject
            .strip_prefix("Merge pull request #")
            .and_then(|rest| rest.split_once(' '))
            .map(|(n, _)| n)
            .or_else(|| {
                subject
                    .strip_suffix(')')
                    .and_then(|s| s.rsplit_once("(#"))
                    .map(|(_, n)| n)
            });
        let (Some(number), Some(at)) = (number.and_then(|n| n.parse::<u64>().ok()), utc(at)) else {
            continue;
        };
        out.entry(number).or_insert(at);
    }
    out
}

fn gh_closed_at(pr: u64, cwd: &str) -> Option<String> {
    let path = format!("repos/{{owner}}/{{repo}}/pulls/{pr}");
    let out = crate::loopcheck::bounded_read(
        "gh".as_ref(),
        &["api", &path],
        Path::new(cwd),
        "gc-sweep",
        Duration::from_secs(30),
    )
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let pr: Value = serde_json::from_slice(&out.stdout).ok()?;
    if pr.get("merged_at").and_then(Value::as_str).is_some() {
        return None;
    }
    utc(pr.get("closed_at").and_then(Value::as_str)?)
}

pub(crate) fn utc(at: &str) -> Option<String> {
    let parsed = chrono::DateTime::parse_from_rfc3339(at).ok()?;
    Some(
        parsed
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: &str, extra: Value, sessions: Value) -> Value {
        let mut row = json!({"id": id, "title": id, "status": "done", "sessions": sessions});
        for (key, value) in extra.as_object().unwrap() {
            row[key] = value.clone();
        }
        row
    }

    #[test]
    fn merge_commits_read_both_subjects_in_utc() {
        let log = "2026-09-12T21:33:22-07:00\tMerge pull request #1891 from o/b\n\
                   2026-09-10T08:00:00+00:00\tfix the thing (#1700)\n\
                   2026-09-09T08:00:00+00:00\tplain commit\n\
                   2026-09-08T08:00:00+00:00\tMerge pull request #1891 from o/old\n";
        let times = parse_merge_commits(log);
        assert_eq!(
            times.get(&1891).map(String::as_str),
            Some("2026-09-13T04:33:22Z")
        );
        assert_eq!(
            times.get(&1700).map(String::as_str),
            Some("2026-09-10T08:00:00Z")
        );
        assert_eq!(times.len(), 2);
    }

    #[test]
    fn ship_ends_only_on_a_recorded_merge_or_close() {
        let open = json!([{"phase": "ship", "harness": "claude", "session_id": "s", "started_at": "2026-09-01T00:00:00Z"}]);
        let ended = json!([{"phase": "ship", "harness": "claude", "session_id": "s", "ended_at": "2026-09-02T00:00:00Z"}]);
        let entries = vec![
            node(
                "x-m",
                json!({"pr_number": 1, "cwd": "/r", "merge_status": "merged"}),
                open.clone(),
            ),
            node(
                "x-c",
                json!({"pr_number": 2, "cwd": "/r", "merge_status": "closed"}),
                open.clone(),
            ),
            node("x-o", json!({"pr_number": 3, "cwd": "/r"}), open.clone()),
            node(
                "x-e",
                json!({"pr_number": 4, "cwd": "/r", "merge_status": "merged"}),
                ended,
            ),
            node(
                "x-u",
                json!({"pr_number": 5, "cwd": "/r", "merge_status": "merged"}),
                open,
            ),
        ];
        let mut asked = Vec::new();
        let plan = plan_ship_ends(&entries, &mut |cwd, pr, merged| {
            asked.push((cwd.to_string(), pr, merged));
            (pr != 5).then(|| format!("t{pr}"))
        });
        assert_eq!(
            plan,
            vec![
                ("x-m".to_string(), "t1".to_string(), "merge-commit"),
                ("x-c".to_string(), "t2".to_string(), "pr-closed"),
            ]
        );
        // The unmerged node and the ended row cost no lookup.
        assert_eq!(asked.len(), 3);
    }

    #[test]
    fn a_retired_session_closes_its_think_blueprint_and_review_rows_only() {
        let rows = |sid: &str| {
            json!([
                {"phase": "think", "harness": "claude", "session_id": sid, "started_at": "2026-09-01T00:00:00Z"},
                {"phase": "review", "harness": "claude", "session_id": sid, "started_at": "2026-09-01T00:00:00Z"},
                {"phase": "execute", "harness": "claude", "session_id": sid, "started_at": "2026-09-01T00:00:00Z"},
                {"phase": "ship", "harness": "claude", "session_id": sid, "started_at": "2026-09-01T00:00:00Z"},
                {"phase": "blueprint", "harness": "claude", "session_id": sid, "ended_at": "2026-09-01T00:00:00Z"},
                {"phase": "blueprint", "harness": "claude", "session_id": sid}
            ])
        };
        let entries = vec![
            node("x-a", json!({}), rows("S-1")),
            node("x-b", json!({}), rows("s-2")),
        ];
        let retired: HashSet<String> = ["s-1".to_string()].into();
        let plan = plan_retired_closes(&entries, &retired);
        let phases: Vec<(&str, &str)> = plan.iter().map(|(n, _, _, p)| (n.as_str(), *p)).collect();
        assert_eq!(phases, vec![("x-a", "think"), ("x-a", "review")]);
        assert_eq!(plan[0].2, "S-1", "the close names the row's own session id");
    }

    #[test]
    fn phase_end_fills_every_open_ship_row_once() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        std::fs::write(
            &graph,
            json!({"entries": [{
                "id": "x-s", "slug": "s", "type": "feature", "title": "s",
                "status": "idea", "priority": "p2",
                "sessions": [
                    {"phase": "ship", "harness": "claude", "session_id": "a", "started_at": "2026-09-01T00:00:00Z"},
                    {"phase": "ship", "harness": "codex", "session_id": "b", "started_at": "2026-09-01T01:00:00Z"},
                    {"phase": "execute", "harness": "claude", "session_id": "a", "started_at": "2026-09-01T00:00:00Z"}
                ]
            }]})
            .to_string(),
        )
        .unwrap();
        let store = Store::new(&graph);
        let at = "2026-09-02T00:00:00Z";
        assert!(close_ship_rows_at(&store, &[("x-s".into(), at.into())], "merge").is_empty());
        let row = api::rows(&store).unwrap().remove(0);
        assert_eq!(
            stamps(&row),
            vec![
                ("ship", Some("2026-09-01T00:00:00Z"), Some(at)),
                ("ship", Some("2026-09-01T01:00:00Z"), Some(at)),
                ("execute", Some("2026-09-01T00:00:00Z"), None),
            ]
        );
        assert_eq!(row["sessions"][0]["ended_by"], json!("merge"));
        let again = [(
            "x-s".to_string(),
            "2026-09-03T00:00:00Z".to_string(),
            "merge",
        )];
        assert_eq!(api::phase_end(&store, "ship", &again).unwrap(), 0);
    }

    /// (phase, started_at, ended_at) per session row.
    fn stamps(row: &Value) -> Vec<(&str, Option<&str>, Option<&str>)> {
        row["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["phase"].as_str().unwrap(),
                    s["started_at"].as_str(),
                    s["ended_at"].as_str(),
                )
            })
            .collect()
    }

    #[test]
    fn a_retired_session_leaves_its_think_and_review_rows_with_both_stamps() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        std::fs::write(
            &graph,
            json!({"entries": [{
                "id": "x-t", "title": "t", "status": "idea", "priority": "p2",
                "sessions": [
                    {"phase": "think", "harness": "claude", "session_id": "gone-1", "started_at": "2026-09-01T00:00:00Z"},
                    {"phase": "review", "harness": "claude", "session_id": "gone-1", "started_at": "2026-09-01T02:00:00Z"},
                    {"phase": "think", "harness": "claude", "session_id": "alive-2", "started_at": "2026-09-01T03:00:00Z"}
                ]
            }]})
            .to_string(),
        )
        .unwrap();
        let store = Store::new(&graph);
        let retired: HashSet<String> = ["gone-1".to_string()].into();
        assert!(close_session_rows(&store, &retired).is_empty());
        let row = api::rows(&store).unwrap().remove(0);
        let rows = stamps(&row);
        for (phase, started, ended) in &rows[..2] {
            assert!(
                started.is_some() && ended.is_some(),
                "{phase} row: {rows:?}"
            );
        }
        assert_eq!(rows[2].2, None, "a live session's row stays open");
        assert_eq!(row["sessions"][0]["ended_by"], json!("reap-sweep"));
    }
}
