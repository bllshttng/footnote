//! Attribute one process-table snapshot to live agent sessions.
//!
//! This module deliberately works on borrowed census rows.  A machine tick
//! performs one process read and passes that same table to both the machine
//! counters and this attribution pass.

use crate::census::ProcRow;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRoot {
    pub session_id: String,
    pub harness: String,
    pub node: Option<String>,
    pub pid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PhaseRow {
    pub session_id: String,
    pub node: Option<String>,
    pub stage: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Attribution {
    pub sessions: Vec<Value>,
    pub unresolved: Vec<Value>,
    pub top_rss: Vec<Value>,
}

fn parent_map(table: &[ProcRow]) -> HashMap<u32, u32> {
    table.iter().map(|row| (row.pid, row.ppid)).collect()
}

fn root_for(pid: u32, parents: &HashMap<u32, u32>, roots: &HashMap<u32, usize>) -> Option<usize> {
    let mut current = pid;
    let mut seen = HashSet::new();
    for _ in 0..64 {
        if let Some(root) = roots.get(&current) {
            return Some(*root);
        }
        if !seen.insert(current) {
            return None;
        }
        current = *parents.get(&current)?;
        if current <= 1 {
            return None;
        }
    }
    None
}

fn basename(command: &str) -> &str {
    command
        .split_whitespace()
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("")
}

fn bucket(row: &ProcRow) -> Option<&'static str> {
    if crate::test_run::is_cargo_row(row)
        || crate::test_run::is_compile(row)
        || crate::orphan_reap::is_deps_test_binary(&row.command)
    {
        return Some("cargo");
    }
    let argv: Vec<String> = row.command.split_whitespace().map(str::to_string).collect();
    if matches!(basename(&row.command), "pytest" | "py.test")
        || (crate::hook::test_run_guard::is_python(basename(&row.command))
            && crate::hook::test_run_guard::has_dash_m_module(&argv, "pytest"))
    {
        return Some("pytest");
    }
    None
}

fn stage_for(
    root: &SessionRoot,
    phases: &[PhaseRow],
    test_pids: &HashSet<u32>,
    owned: &[&ProcRow],
) -> (Option<String>, Option<String>) {
    let is_test =
        test_pids.contains(&root.pid) || owned.iter().any(|row| test_pids.contains(&row.pid));
    if is_test {
        return (Some("test".into()), root.node.clone());
    }
    phases
        .iter()
        .rev()
        .find(|phase| phase.session_id == root.session_id)
        .map(|phase| (phase.stage.clone(), phase.node.clone()))
        .unwrap_or((None, root.node.clone()))
}

pub fn attribute(
    table: &[ProcRow],
    roots: &[SessionRoot],
    phases: &[PhaseRow],
    test_pids: &HashSet<u32>,
) -> Attribution {
    let parents = parent_map(table);
    let root_map: HashMap<u32, usize> = roots
        .iter()
        .enumerate()
        .map(|(i, root)| (root.pid, i))
        .collect();
    let mut owned: Vec<Vec<&ProcRow>> = (0..roots.len()).map(|_| Vec::new()).collect();
    let mut owners = HashMap::new();
    for row in table {
        if let Some(index) = root_for(row.pid, &parents, &root_map) {
            owned[index].push(row);
            owners.insert(row.pid, roots[index].session_id.clone());
        }
    }
    let mut sessions = Vec::new();
    let mut unresolved = Vec::new();
    for (index, root) in roots.iter().enumerate() {
        if owned[index].is_empty() {
            unresolved.push(json!({
                "session_id": root.session_id,
                "node": root.node,
                "stage": null,
                "reason": "root pid not in process table"
            }));
            continue;
        }
        let (stage, node) = stage_for(root, phases, test_pids, &owned[index]);
        let mut row = json!({
            "session_id": root.session_id,
            "harness": root.harness,
            "node": node,
            "stage": stage,
            "procs": owned[index].len(),
            "rss_mb": owned[index].iter().map(|r| r.rss_kb).sum::<u64>() as f64 / 1024.0,
            "cpu_pct": owned[index].iter().map(|r| r.cpu_pct).sum::<f64>(),
        });
        let mut buckets: MapBuckets = BTreeMap::new();
        for process in &owned[index] {
            if let Some(name) = bucket(process) {
                let item = buckets.entry(name).or_default();
                item.0 += 1;
                item.1 += process.rss_kb;
                item.2 += process.cpu_pct;
            }
        }
        if !buckets.is_empty() {
            let mut values = serde_json::Map::new();
            for (name, (procs, rss, cpu)) in buckets {
                values.insert(
                    name.into(),
                    json!({ "procs": procs, "rss_mb": rss as f64 / 1024.0, "cpu_pct": cpu }),
                );
            }
            row["buckets"] = Value::Object(values);
        }
        sessions.push(row);
    }
    Attribution {
        sessions,
        unresolved,
        top_rss: top_by_rss(table, &owners, 10),
    }
}

type MapBuckets = BTreeMap<&'static str, (u64, u64, f64)>;

pub fn tree_rss(table: &[ProcRow], pids: &[u32]) -> BTreeMap<u32, u64> {
    let parents = parent_map(table);
    let roots: HashMap<u32, usize> = pids.iter().enumerate().map(|(i, pid)| (*pid, i)).collect();
    let mut totals: BTreeMap<u32, u64> = pids
        .iter()
        .filter(|pid| table.iter().any(|row| row.pid == **pid))
        .map(|pid| (*pid, 0))
        .collect();
    for row in table {
        if let Some(index) = root_for(row.pid, &parents, &roots) {
            if let Some(root) = pids.get(index) {
                *totals.entry(*root).or_default() += row.rss_kb / 1024;
            }
        }
    }
    totals
}

pub fn top_by_rss(table: &[ProcRow], owners: &HashMap<u32, String>, limit: usize) -> Vec<Value> {
    let mut rows: Vec<&ProcRow> = table.iter().collect();
    rows.sort_by(|a, b| b.rss_kb.cmp(&a.rss_kb).then(a.pid.cmp(&b.pid)));
    rows.into_iter()
        .take(limit)
        .map(|row| {
            json!({
                "name": basename(&row.command),
                "pid": row.pid,
                "rss_mb": row.rss_kb as f64 / 1024.0,
                "cpu_pct": row.cpu_pct,
                "session_id": owners.get(&row.pid),
            })
        })
        .collect()
}

pub fn footprint_mb(pids: &[u32]) -> (Option<f64>, u64) {
    #[cfg(target_os = "macos")]
    {
        let mut total_bytes = 0u64;
        let mut unread = 0u64;
        for pid in pids {
            let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
            let mut buffer: libc::rusage_info_t = (&mut info as *mut libc::rusage_info_v4).cast();
            let result = unsafe {
                libc::proc_pid_rusage(*pid as libc::c_int, libc::RUSAGE_INFO_V4, &mut buffer)
            };
            if result == 0 {
                total_bytes = total_bytes.saturating_add(info.ri_phys_footprint);
            } else {
                unread = unread.saturating_add(1);
            }
        }
        (Some(total_bytes as f64 / 1024.0 / 1024.0), unread)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pids;
        (None, 0)
    }
}

pub fn session_roots(home: &crate::paths::AgentsHome, table: &[ProcRow]) -> Vec<SessionRoot> {
    let mut roots = BTreeMap::<String, SessionRoot>::new();
    let mut warnings = Vec::new();
    for entry in crate::spawn_gate::live_rows(&home.registry_json(), &mut warnings) {
        let Some(session_id) = entry.harness_session_id.or(entry.session_id) else {
            continue;
        };
        let Some(pid) = entry.pid else { continue };
        roots.entry(session_id.clone()).or_insert(SessionRoot {
            session_id,
            harness: entry
                .harness
                .or(entry.provider)
                .unwrap_or_else(|| "unknown".into()),
            node: entry.node,
            pid,
        });
    }
    if let Ok(roster) = crate::claude_roster::ClaudeRoster::load_default() {
        for worker in roster.workers_deduped() {
            let Some(pid) = worker.pid else { continue };
            if !crate::daemon::pid_is_ours(pid, worker.proc_start) {
                continue;
            }
            let root_pid = worker
                .repl_pid
                .filter(|candidate| table.iter().any(|row| row.pid == *candidate))
                .unwrap_or(pid);
            roots
                .entry(worker.session_id.clone())
                .or_insert(SessionRoot {
                    session_id: worker.session_id.clone(),
                    harness: "claude".into(),
                    node: None,
                    pid: root_pid,
                });
        }
    }
    roots.into_values().collect()
}

fn phase_rows(home: &crate::paths::AgentsHome) -> Result<Vec<PhaseRow>, String> {
    let path = crate::gc_sweep::graph_path(home);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let rows = crate::backlog::api::rows(&crate::backlog::api::Store::new(&path))
        .map_err(|error| format!("{}: {}", path.display(), error.0))?;
    let mut phases = Vec::new();
    for row in rows {
        for phase in ["think", "blueprint", "do", "review", "ship"] {
            if !crate::graph_store::is_open_phase_row(&row, phase) {
                continue;
            }
            let Some(session_id) = row.get("session_id").and_then(Value::as_str) else {
                continue;
            };
            phases.push(PhaseRow {
                session_id: session_id.to_string(),
                node: row.get("node").and_then(Value::as_str).map(str::to_string),
                stage: Some(phase.to_string()),
            });
        }
    }
    Ok(phases)
}

fn test_pids() -> HashSet<u32> {
    crate::claims::list(Some("test:"), None, false)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|claim| {
            claim
                .holder
                .strip_prefix("test-run:")?
                .split(':')
                .next()?
                .parse()
                .ok()
        })
        .collect()
}

pub fn price(home: &Path, table: &[ProcRow]) -> Result<Value, String> {
    let agents_home = crate::paths::AgentsHome::from_env();
    let roots = session_roots(&agents_home, table);
    let phases = phase_rows(&agents_home)?;
    let test_pids = test_pids();
    let _ = home;
    let out = attribute(table, &roots, &phases, &test_pids);
    Ok(json!({ "sessions": out.sessions, "unresolved": out.unresolved, "top_rss": out.top_rss }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: u32, ppid: u32, command: &str, rss_kb: u64) -> ProcRow {
        ProcRow {
            pid,
            ppid,
            state: 'S',
            elapsed_s: 1,
            cpu_pct: 1.0,
            rss_kb,
            command: command.into(),
        }
    }

    #[test]
    fn attributes_roots_and_descendants_without_crossing_cycles() {
        let table = vec![
            row(100, 1, "session", 1024),
            row(201, 100, "cargo test", 2048),
            row(202, 201, "rustc", 1024),
            row(300, 1, "fseventsd", 28 * 1024 * 1024),
        ];
        let roots = vec![SessionRoot {
            session_id: "a".into(),
            harness: "claude".into(),
            node: Some("x-a".into()),
            pid: 100,
        }];
        let out = attribute(
            &table,
            &roots,
            &[PhaseRow {
                session_id: "a".into(),
                node: Some("x-a".into()),
                stage: Some("blueprint".into()),
            }],
            &HashSet::new(),
        );
        assert_eq!(out.sessions.len(), 1);
        assert_eq!(out.sessions[0]["stage"], "blueprint");
        assert_eq!(out.sessions[0]["buckets"]["cargo"]["procs"], 2);
        assert_eq!(out.top_rss[0]["name"], "fseventsd");
    }

    #[test]
    fn tree_rss_sums_recursive_children() {
        let table = vec![
            row(10, 1, "root", 1024),
            row(11, 10, "child", 2048),
            row(12, 11, "leaf", 1024),
        ];
        assert_eq!(tree_rss(&table, &[10]).get(&10), Some(&4));
    }
}
