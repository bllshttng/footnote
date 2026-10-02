//! Test all-stop: the process half of a breaker that holds `tests`.
//!
//! The breaker's doors stop NEW test runs; this module reaches the ones
//! already running. While the machine-wide record holds `tests`, every cargo
//! test, nextest and pytest run under a live registry row gets SIGSTOP with
//! its whole process subtree, and the paused incarnations are recorded in
//! `agents/test-pause.json`. When the hold lifts, exactly those incarnations
//! get SIGCONT and the file goes. The first pass of a hold announces it on
//! the bus; the lift announces the all-clear under the same subject, so the
//! all-clear supersedes the standing hold line.
//!
//! [`reconcile`] is idempotent and is the one entry. The incident verb runs
//! it after every machine-wide transition, and the daemon's machine tick
//! runs it every interval, so an expired TTL still resumes the paused
//! processes and a bare pytest started mid-hold (no door gates it) pauses at
//! the next tick. An unreadable breaker changes nothing in either direction.
//!
//! Registry rows are the fleet: spawn writes the row, so a test under one is
//! a test a fleet worker started. The user's own terminal is never a row.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::census::ProcRow;
use crate::fleet_incident::{self, Verdict};
use crate::paths::AgentsHome;

pub const ANNOUNCE_SUBJECT: &str = "test-hold";

#[derive(Debug, Default, Serialize, Deserialize)]
struct PauseState {
    generation: u64,
    /// `(pid, birth)`: SIGCONT goes only to the same incarnation, never to a
    /// recycled pid.
    paused: Vec<(u32, u64)>,
    #[serde(default)]
    announced: Option<String>,
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub paused: usize,
    pub resumed: usize,
    pub announced: Option<String>,
    pub announce_error: Option<String>,
}

impl Outcome {
    pub fn line(&self) -> Option<String> {
        if self.paused == 0
            && self.resumed == 0
            && self.announced.is_none()
            && self.announce_error.is_none()
        {
            return None;
        }
        let mut line = format!("tests: paused {}, resumed {}", self.paused, self.resumed);
        match (&self.announced, &self.announce_error) {
            (Some(id), _) => line.push_str(&format!("; announced {id}")),
            (None, Some(error)) => line.push_str(&format!("; announcement failed: {error}")),
            (None, None) => {}
        }
        Some(line)
    }
}

/// True when `command` starts a test run this hold pauses: `cargo test`,
/// `cargo nextest run`, `pytest`, or a python running pytest as a script or
/// with `-m pytest`.
pub(crate) fn is_test_root(command: &str) -> bool {
    let argv: Vec<String> = command.split_whitespace().map(str::to_string).collect();
    if crate::test_run::cargo_test_args_start(&argv).is_some() {
        return true;
    }
    let base = |token: &str| token.rsplit('/').next().unwrap_or("").to_string();
    let Some(program) = argv.first().map(|a| base(a)) else {
        return false;
    };
    if matches!(program.as_str(), "pytest" | "py.test") {
        return true;
    }
    crate::hook::test_run_guard::is_python(&program)
        && (argv
            .get(1)
            .is_some_and(|a| matches!(base(a).as_str(), "pytest" | "py.test"))
            || crate::hook::test_run_guard::has_dash_m_module(&argv, "pytest"))
}

/// Every pid a hold pauses: each test root under a fleet pid, plus its
/// whole subtree. `spare` (this process and its ancestors) is never picked,
/// so the verb can never stop the shell that ran it.
pub(crate) fn pick(table: &[ProcRow], fleet: &HashSet<u32>, spare: &HashSet<u32>) -> Vec<u32> {
    let parents: HashMap<u32, u32> = table.iter().map(|r| (r.pid, r.ppid)).collect();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for row in table {
        children.entry(row.ppid).or_default().push(row.pid);
    }
    let under_fleet = |pid: u32| {
        let mut current = pid;
        for _ in 0..64 {
            if fleet.contains(&current) {
                return true;
            }
            match parents.get(&current) {
                Some(&parent) if parent > 1 => current = parent,
                _ => return false,
            }
        }
        false
    };
    let mut picked = Vec::new();
    let mut seen = HashSet::new();
    let mut stack: Vec<u32> = table
        .iter()
        .filter(|r| r.state != 'Z' && is_test_root(&r.command) && under_fleet(r.pid))
        .map(|r| r.pid)
        .collect();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) || spare.contains(&pid) {
            continue;
        }
        picked.push(pid);
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids);
        }
    }
    picked.sort_unstable();
    picked
}

/// The pids of live registry rows. A pid-less row (a bg thread) resolves
/// through the claude roster by its transport short id.
fn fleet_pids(home: &AgentsHome) -> HashSet<u32> {
    let mut warnings = Vec::new();
    let rows = crate::spawn_gate::live_rows(&home.registry_json(), &mut warnings);
    let roster = crate::claude_roster::ClaudeRoster::load_default().ok();
    let mut pids = HashSet::new();
    for row in rows {
        if let Some(pid) = row.pid {
            pids.insert(pid);
            continue;
        }
        let (Some(short), Some(roster)) = (row.transport_short(), roster.as_ref()) else {
            continue;
        };
        for worker in roster.workers_deduped() {
            if worker.short_id() == short {
                pids.extend(worker.repl_pid.or(worker.pid));
            }
        }
    }
    pids
}

fn spare_pids(table: &[ProcRow]) -> HashSet<u32> {
    let parents: HashMap<u32, u32> = table.iter().map(|r| (r.pid, r.ppid)).collect();
    let mut spare = HashSet::new();
    let mut current = std::process::id();
    while current > 1 && spare.insert(current) {
        match parents.get(&current) {
            Some(&parent) => current = parent,
            None => break,
        }
    }
    spare
}

fn signal(pid: u32, sig: libc::c_int) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, sig) == 0 }
}

fn read_state(path: &Path) -> Option<PauseState> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_state(path: &Path, state: &PauseState) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_string(state).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, body).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

fn announce(outcome: &mut Outcome, body: &str) {
    match crate::announce::announce_all("fno/fleet-incident", ANNOUNCE_SUBJECT, body) {
        Ok(id) => outcome.announced = Some(id),
        Err(error) => outcome.announce_error = Some(error),
    }
}

/// Make the running tests follow the machine-wide record: paused while it
/// holds `tests`, resumed once it does not.
pub fn reconcile(home: &AgentsHome) -> Result<Outcome, String> {
    let path = home.test_pause_json();
    let lock_path = path.with_extension("json.lock");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("cannot open {}: {e}", lock_path.display()))?;
    lock.lock()
        .map_err(|e| format!("cannot lock {}: {e}", lock_path.display()))?;
    let result = reconcile_locked(home, &path);
    let _ = lock.unlock();
    result
}

fn reconcile_locked(home: &AgentsHome, path: &Path) -> Result<Outcome, String> {
    let prior = read_state(path);
    let mut outcome = Outcome::default();
    let record = match fleet_incident::read_at(&fleet_incident::fleet_stop_path(home)) {
        Verdict::Unavailable(_) => return Ok(outcome),
        Verdict::Stopped(r) if r.holds_scope("tests") => Some(r),
        Verdict::Stopped(_) | Verdict::Clear(_) => None,
    };
    let Some(record) = record else {
        let Some(state) = prior else {
            return Ok(outcome);
        };
        for (pid, birth) in state.paused {
            if crate::daemon::process_start_time(pid) == Some(birth) && signal(pid, libc::SIGCONT) {
                outcome.resumed += 1;
            }
        }
        announce(
            &mut outcome,
            "All clear: the test hold is lifted. Tests may run again.",
        );
        std::fs::remove_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
        return Ok(outcome);
    };
    let first = prior.is_none();
    let mut state = prior.unwrap_or_default();
    state.generation = record.generation;
    let fleet = fleet_pids(home);
    let mut done: HashSet<u32> = state.paused.iter().map(|(pid, _)| *pid).collect();
    // A cargo can fork a compile between the scan and its stop; rescan until
    // a pass finds nothing new.
    for _ in 0..3 {
        let (table, _) = crate::census::process_table();
        let spare = spare_pids(&table);
        let fresh: Vec<u32> = pick(&table, &fleet, &spare)
            .into_iter()
            .filter(|pid| !done.contains(pid))
            .collect();
        if fresh.is_empty() {
            break;
        }
        for pid in fresh {
            done.insert(pid);
            let Some(birth) = crate::daemon::process_start_time(pid) else {
                continue;
            };
            if signal(pid, libc::SIGSTOP) {
                state.paused.push((pid, birth));
                outcome.paused += 1;
            }
        }
    }
    if first {
        // The machine arm's reason is already a plain sentence; a person's
        // stop names who held the tests and why.
        let lead = if record.changed_by == fleet_incident::MACHINE_ORIGIN {
            format!(
                "{}. Tests are paused until it cools down. New workers pause too if it stays this busy.",
                record.reason
            )
        } else {
            format!("Tests are held by {} ({}).", record.changed_by, record.reason)
        };
        announce(
            &mut outcome,
            &format!("{lead} Keep coding. Do not rerun tests. Wait for the all-clear."),
        );
        state.announced = outcome.announced.clone();
    }
    write_state(path, &state)?;
    Ok(outcome)
}

/// The machine arm's first move on a runaway: arm a `tests`-only stop that
/// expires after `ttl_secs` and pause the running tests. `Ok(None)` when a
/// stop is already armed or unreadable; the arm then brakes spawns as before.
pub fn hold_for_runaway(
    home: &AgentsHome,
    reason: &str,
    ttl_secs: u64,
) -> Result<Option<String>, String> {
    let path = fleet_incident::fleet_stop_path(home);
    if !matches!(fleet_incident::read_at(&path), Verdict::Clear(_)) {
        return Ok(None);
    }
    let record = fleet_incident::write_transition_with_metadata(
        &path,
        "stopped",
        Some(reason),
        Some(fleet_incident::MACHINE_ORIGIN),
        vec!["tests".to_string()],
        fleet_incident::RecordMetadata {
            expires_at: Some(fleet_incident::expires_after(ttl_secs * 1000)?),
            origin: Some(fleet_incident::MACHINE_ORIGIN.to_string()),
            ..Default::default()
        },
    )?;
    let outcome = reconcile(home)?;
    Ok(Some(format!(
        "tests held first (generation {}, {} paused)",
        record.generation, outcome.paused
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::census::test_proc_row as row;

    #[test]
    fn a_hold_pauses_fleet_test_subtrees_and_spares_the_rest() {
        assert!(is_test_root("/Users/u/.cargo/bin/cargo test -p fno-agents"));
        assert!(is_test_root("cargo +nightly nextest run"));
        assert!(is_test_root("/venv/bin/python3 /venv/bin/pytest cli/tests"));
        assert!(is_test_root("python3 -m pytest -x"));
        assert!(!is_test_root("cargo build -p fno"));
        assert!(!is_test_root("fno-agents test-run -- cargo test"));
        assert!(!is_test_root("python3 script.py pytest"));
        let table = vec![
            row(10, 1, "claude"),
            row(11, 10, "bash"),
            row(12, 11, "cargo test -p fno-agents"),
            row(13, 12, "rustc --crate-name fno_agents"),
            row(14, 12, "target/debug/deps/fno_agents-abc"),
            row(20, 1, "zsh"),
            row(21, 20, "cargo test"),
            row(30, 10, "python3 -m pytest"),
        ];
        let fleet: HashSet<u32> = [10].into();
        assert_eq!(pick(&table, &fleet, &HashSet::new()), vec![12, 13, 14, 30]);
        let spare: HashSet<u32> = [30].into();
        assert_eq!(pick(&table, &fleet, &spare), vec![12, 13, 14]);
    }
}
