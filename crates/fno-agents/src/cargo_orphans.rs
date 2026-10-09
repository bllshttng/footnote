//! Cargo admission holders whose owner is gone.
//!
//! An agent cargo holds its run slot and `build:cargo` keyed to its own pid,
//! with no TTL. A reaped session leaves its cargo running under pid 1, so
//! the pid stays live and the slot never frees: on 2026-10-08 one orphan held
//! the queue for over five hours. At acquire time an agent cargo records its
//! ancestor chain, each pid with its start time, in the claim metadata. A
//! later cargo that finds a holder whose chain has a dead link, or a hold
//! older than the cap, ends that holder's cargo tree and frees the claim.
//! A user build records no chain, so it is never swept (law d-705a00a3).

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::Path;

const OWNER_CHAIN: &str = "owner_chain";
const DEFAULT_MAX_HOLD_SECS: i64 = 3600;
/// Start times come from one source, but allow a second of drift between
/// two reads of the same process before calling it a different one.
const START_SLACK_MS: i64 = 1000;

/// The claim metadata an agent cargo writes at acquire: its ancestors,
/// nearest first, each as `[pid, start_ms]`. `None` when the table cannot
/// place the cargo.
pub(crate) fn owner_metadata(cargo_pid: u32) -> Option<Map<String, Value>> {
    let (table, _) = crate::census::process_table();
    let entries: Vec<Value> = ancestor_chain(&table, cargo_pid)
        .into_iter()
        .filter_map(|pid| match crate::claims::probe_pid(pid as i32) {
            crate::claims::PidProbe::Created(start_ms) => Some(json!([pid, start_ms])),
            _ => None,
        })
        .collect();
    if entries.is_empty() {
        return None;
    }
    let mut map = Map::new();
    map.insert(OWNER_CHAIN.to_string(), Value::Array(entries));
    Some(map)
}

/// Ancestors of `pid`, nearest first. The walk stops below the first
/// process whose parent is pid 1: that is a top-level daemon (the claude
/// daemon, a pane keeper) whose restart leaves its sessions running.
fn ancestor_chain(table: &[crate::census::ProcRow], pid: u32) -> Vec<u32> {
    let parent: HashMap<u32, u32> = table.iter().map(|row| (row.pid, row.ppid)).collect();
    let mut chain = Vec::new();
    let mut current = pid;
    for _ in 0..32 {
        match parent.get(&current) {
            Some(&next) if next > 1 && parent.get(&next).is_some_and(|&up| up > 1) => {
                chain.push(next);
                current = next;
            }
            _ => break,
        }
    }
    chain
}

/// Why a holder no longer has an owner, or `None` while it does. Only a
/// claim that carries an owner chain is judged.
fn orphan_reason(
    metadata: &Map<String, Value>,
    acquired_at: i64,
    now_ms: i64,
    max_hold_ms: i64,
    probe: &dyn Fn(i32) -> crate::claims::PidProbe,
) -> Option<String> {
    let chain = metadata.get(OWNER_CHAIN)?.as_array()?;
    if now_ms - acquired_at > max_hold_ms {
        return Some(format!(
            "held {}m, over the {}m cap",
            (now_ms - acquired_at) / 60_000,
            max_hold_ms / 60_000
        ));
    }
    for entry in chain {
        let (Some(pid), Some(start_ms)) = (
            entry.get(0).and_then(Value::as_i64),
            entry.get(1).and_then(Value::as_i64),
        ) else {
            continue;
        };
        match probe(pid as i32) {
            crate::claims::PidProbe::Created(live) if (live - start_ms).abs() <= START_SLACK_MS => {
            }
            crate::claims::PidProbe::Refused => {}
            _ => return Some(format!("owner pid {pid} is gone")),
        }
    }
    None
}

fn max_hold_ms() -> i64 {
    std::env::var("FNO_CARGO_MAX_HOLD_SECS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_MAX_HOLD_SECS)
        * 1000
}

/// Every process under `root`, by its ppid chain.
fn descendants(table: &[crate::census::ProcRow], root: u32) -> Vec<u32> {
    let parent: HashMap<u32, u32> = table.iter().map(|row| (row.pid, row.ppid)).collect();
    table
        .iter()
        .filter(|row| {
            let mut current = row.ppid;
            for _ in 0..64 {
                if current == root {
                    return true;
                }
                match parent.get(&current) {
                    Some(&next) if current > 1 => current = next,
                    _ => return false,
                }
            }
            false
        })
        .map(|row| row.pid)
        .collect()
}

/// SIGTERM the holder cargo and every process under it. The cargo must
/// still be the incarnation that took the claim: one started after
/// `acquired_at` is a recycled pid and takes nothing.
fn end_cargo_tree(cargo_pid: u32, acquired_at: i64) {
    match crate::claims::probe_pid(cargo_pid as i32) {
        crate::claims::PidProbe::Created(start_ms) if start_ms <= acquired_at => {}
        _ => return,
    }
    let (table, _) = crate::census::process_table();
    let mut members = descendants(&table, cargo_pid);
    members.push(cargo_pid);
    for pid in members.into_iter().filter(|pid| *pid > 1) {
        // SAFETY: the pid is the proved holder cargo or a process under it.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
}

/// Free every claim in `keys` whose live holder has lost its owner: end the
/// holder's cargo tree, then release the claim by its exact holder string,
/// so a claim that changed hands meanwhile is left alone.
pub(crate) fn release_orphan_holders(keys: &[String], events_dir: &Path) {
    let now = crate::claims::now_ms();
    let max_hold = max_hold_ms();
    for key in keys {
        let (crate::claims::ClaimState::Live, Some(rec)) = crate::claims::status(key, None) else {
            continue;
        };
        let Some(reason) = orphan_reason(
            &rec.metadata,
            rec.acquired_at,
            now,
            max_hold,
            &crate::claims::probe_pid,
        ) else {
            continue;
        };
        if let Some(pid) = rec.pid.filter(|pid| *pid > 1) {
            end_cargo_tree(pid as u32, rec.acquired_at);
        }
        if crate::claims::release(key, &rec.holder, None, Some(events_dir)).is_ok() {
            eprintln!(
                "cargo admission: freed {key} from {} ({reason}); its cargo tree got SIGTERM",
                rec.holder
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::PidProbe;

    fn row(pid: u32, ppid: u32) -> crate::census::ProcRow {
        crate::census::ProcRow {
            pid,
            ppid,
            state: 'S',
            elapsed_s: 0,
            cpu_pct: 0.0,
            rss_kb: 0,
            command: String::new(),
        }
    }

    #[test]
    fn owner_chain_judges_dead_links_the_cap_and_stops_below_the_daemon() {
        // daemon 10 (ppid 1) -> pty host 20 -> session 30 -> shell 40 -> cargo 50 -> rustc 60
        let table = [
            row(10, 1),
            row(20, 10),
            row(30, 20),
            row(40, 30),
            row(50, 40),
            row(60, 50),
        ];
        assert_eq!(ancestor_chain(&table, 50), vec![40, 30, 20]);
        assert_eq!(descendants(&table, 40), vec![50, 60]);

        let mut metadata = Map::new();
        metadata.insert(OWNER_CHAIN.to_string(), json!([[40, 1_000], [30, 900]]));
        let hour = 3_600_000;
        let alive = |pid: i32| PidProbe::Created(if pid == 40 { 1_000 } else { 900 });
        assert_eq!(orphan_reason(&metadata, 0, 60_000, hour, &alive), None);

        let session_gone = |pid: i32| {
            if pid == 30 {
                PidProbe::Absent
            } else {
                PidProbe::Created(1_000)
            }
        };
        assert_eq!(
            orphan_reason(&metadata, 0, 60_000, hour, &session_gone).as_deref(),
            Some("owner pid 30 is gone")
        );
        let recycled = |pid: i32| PidProbe::Created(if pid == 40 { 1_000 } else { 50_000 });
        assert!(orphan_reason(&metadata, 0, 60_000, hour, &recycled).is_some());
        assert_eq!(
            orphan_reason(&metadata, 0, hour + 60_000, hour, &alive).as_deref(),
            Some("held 61m, over the 60m cap")
        );
        // A user build records no chain and is never judged, however long it holds.
        assert_eq!(
            orphan_reason(&Map::new(), 0, 10 * hour, hour, &session_gone),
            None
        );
    }
}
