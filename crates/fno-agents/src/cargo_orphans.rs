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
/// Set when the cargo already ran under pid 1 at acquire: it has no owner
/// to record, and without the mark it would hold a claim no sweep can judge.
const NO_OWNER: &str = "no_owner";
/// Set for the sanctioned whole-suite lane, whose run can outlast the cap.
const FULL_SUITE: &str = "full_suite";
const DEFAULT_MAX_HOLD_SECS: i64 = 3600;
/// Start times come from one source, but allow a second of drift between
/// two reads of the same process before calling it a different one.
const START_SLACK_MS: i64 = 1000;

/// The claim metadata an agent cargo writes at acquire: its ancestors,
/// nearest first, each as `[pid, start_ms]`. `None` when the table cannot
/// place the cargo.
pub(crate) fn owner_metadata(cargo_pid: u32) -> Option<Map<String, Value>> {
    let (table, _) = crate::census::process_table();
    let ppid = table.iter().find(|row| row.pid == cargo_pid)?.ppid;
    let entries: Vec<Value> = ancestor_chain(&table, cargo_pid)
        .into_iter()
        .filter_map(|pid| match crate::claims::probe_pid(pid as i32) {
            crate::claims::PidProbe::Created(start_ms) => Some(json!([pid, start_ms])),
            _ => None,
        })
        .collect();
    let mut map = Map::new();
    map.insert(OWNER_CHAIN.to_string(), Value::Array(entries));
    if ppid == 1 {
        map.insert(NO_OWNER.to_string(), Value::Bool(true));
    }
    if crate::test_run::full_suite_lane() {
        map.insert(FULL_SUITE.to_string(), Value::Bool(true));
    }
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
    if metadata.get(NO_OWNER) == Some(&Value::Bool(true)) {
        return Some("it ran under pid 1 when it took the claim".to_string());
    }
    let full_suite = metadata.get(FULL_SUITE) == Some(&Value::Bool(true));
    if !full_suite && now_ms - acquired_at > max_hold_ms {
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

/// SIGTERM the holder cargo and every process under it, and say whether the
/// signal went out. The cargo must still be the incarnation that took the
/// claim: one started after `acquired_at` is a recycled pid and takes nothing.
fn end_cargo_tree(cargo_pid: u32, acquired_at: i64) -> bool {
    match crate::claims::probe_pid(cargo_pid as i32) {
        crate::claims::PidProbe::Created(start_ms) if start_ms <= acquired_at => {}
        _ => return false,
    }
    let (table, _) = crate::census::process_table();
    let mut members = crate::census::descendants(&table, cargo_pid);
    members.push(cargo_pid);
    for pid in members.into_iter().filter(|pid| *pid > 1) {
        // SAFETY: the pid is the proved holder cargo or a process under it.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    true
}

/// Free every claim in `keys` whose live holder has lost its owner: end the
/// holder's cargo tree, then release the claim by its exact holder string,
/// so a claim that changed hands meanwhile is left alone. A holder the
/// signal could not reach keeps its claim: freed, it would re-acquire and
/// compile on beside the next holder.
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
        let Some(pid) = rec.pid.filter(|pid| *pid > 1) else {
            continue;
        };
        if !end_cargo_tree(pid as u32, rec.acquired_at) {
            continue;
        }
        if let Ok(Some(_)) =
            crate::claims::release_with_receipt(key, &rec.holder, None, Some(events_dir))
        {
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
        crate::census::test_proc_row(pid, ppid, "")
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
        assert_eq!(crate::census::descendants(&table, 40), vec![50, 60]);

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
        // The whole-suite lane may outlast the cap while its owner lives.
        let mut full = metadata.clone();
        full.insert(FULL_SUITE.to_string(), Value::Bool(true));
        assert_eq!(orphan_reason(&full, 0, 2 * hour, hour, &alive), None);
        // A cargo already under pid 1 at acquire has no owner to lose.
        let mut no_owner = Map::new();
        no_owner.insert(OWNER_CHAIN.to_string(), json!([]));
        no_owner.insert(NO_OWNER.to_string(), Value::Bool(true));
        assert!(orphan_reason(&no_owner, 0, 60_000, hour, &alive).is_some());
        // A user build records no chain and is never judged, however long it holds.
        assert_eq!(
            orphan_reason(&Map::new(), 0, 10 * hour, hour, &session_gone),
            None
        );
    }
}
