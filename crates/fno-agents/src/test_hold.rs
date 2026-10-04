//! Test all-stop: the process half of a breaker that holds `tests`.
//!
//! The breaker's doors stop NEW test runs; this module reaches the ones
//! already running. While the machine-wide record holds `tests`, every cargo
//! test, nextest and pytest run under a live registry row is killed
//! (SIGKILL) with its whole process subtree. A hold must not pause a run:
//! a SIGSTOPped cargo stays alive, so the pid-anchored claims it holds
//! (`build:cargo`, its `test:cargo-run:N` slot) stay Live and every build
//! queues behind a run that cannot progress (two paused runs once held
//! all three for 2.5h and the canonical fno update stalled behind them).
//! Tests are CI-gated (changed-file runs locally, the whole suite on every
//! PR), so a held run ends instead of waiting. The first pass of a hold
//! announces it on the bus; the lift
//! announces the all-clear under the same subject, so the all-clear
//! supersedes the standing hold line.
//!
//! `agents/test-pause.json` keeps recording what an OLDER build paused, and
//! a lift still SIGCONTs exactly those incarnations, so a hold armed before
//! an upgrade resumes cleanly. Kills need no resume record.
//!
//! [`reconcile`] is idempotent and is the one entry. The incident verb runs
//! it after every machine-wide transition, and the daemon's machine tick
//! runs it every interval, so an expired TTL still resumes what an older
//! build paused and a bare pytest started mid-hold (no door gates it) ends
//! at the next tick. An unreadable breaker changes nothing in either
//! direction.
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
    pub killed: usize,
    pub announced: Option<String>,
    pub announce_error: Option<String>,
}

impl Outcome {
    pub fn line(&self) -> Option<String> {
        if self.paused == 0
            && self.resumed == 0
            && self.killed == 0
            && self.announced.is_none()
            && self.announce_error.is_none()
        {
            return None;
        }
        let mut line = format!(
            "tests: paused {}, resumed {}, killed {}",
            self.paused, self.resumed, self.killed
        );
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

/// True when the pid still is the incarnation the scan picked: the same
/// birth token it carried then. A pid that exited and was reused must never
/// be signaled.
fn same_incarnation(pid: u32, birth: u64) -> bool {
    crate::daemon::process_start_time(pid) == Some(birth)
}

/// End one picked run, birth-verified. SIGKILL, not SIGSTOP: a stopped pid
/// stays alive and every pid-anchored claim it holds reads Live, so builds
/// queue behind a run that cannot progress until the TTL lifts. SIGKILL
/// lands on a stopped process and frees the claims at once.
///
/// A run that leads its own process group (every run the test-run wrapper
/// spawned does) takes the group kill, which reaches a compile forked after
/// the scan: an orphaned child reparents to ppid 1 and the fleet walk stops
/// there, so a per-pid kill would leave it compiling. Any other run is
/// frozen first so it forks nothing new, its live descendants are read and
/// killed, then the run itself; the stop is transient and never resumed.
fn end_run(pid: u32, birth: u64) -> bool {
    if !same_incarnation(pid, birth) {
        return false;
    }
    let group_leader =
        unsafe { libc::getpgid(pid as libc::pid_t) } == pid as libc::pid_t && pid > 1;
    if group_leader && unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) } == 0 {
        return true;
    }
    // Freeze before the second read so the subtree closes under us.
    signal(pid, libc::SIGSTOP);
    let mut ended = false;
    for child in frozen_descendants(pid) {
        // A birth read beside the signal is the liveness proof: a pid that
        // already exited reads no start time and is skipped.
        if crate::daemon::process_start_time(child).is_none() {
            continue;
        }
        if signal(child, libc::SIGKILL) {
            ended = true;
        }
    }
    if signal(pid, libc::SIGKILL) {
        ended = true;
    }
    ended
}

/// Every live pid under `pid` right now: one fresh table read, the frozen
/// root's post-scan forks included. Zombies are skipped: a signal to one is
/// a no-op and its parent reaps it.
fn frozen_descendants(pid: u32) -> Vec<u32> {
    let (table, _) = crate::census::process_table();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for row in &table {
        children.entry(row.ppid).or_default().push(row.pid);
    }
    let mut found = Vec::new();
    let mut stack = vec![pid];
    while let Some(current) = stack.pop() {
        for kid in children.get(&current).into_iter().flatten() {
            if table.iter().any(|r| r.pid == *kid && r.state != 'Z') {
                found.push(*kid);
                stack.push(*kid);
            }
        }
    }
    found
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

/// Make the running tests follow the machine-wide record: killed while it
/// holds `tests`, resumed once it does not (only what an older build paused).
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
            // The scan-time birth token travels with the pid: end_run
            // re-proves it before every signal, so a pid reused in between
            // is never touched.
            let Some(birth) = crate::daemon::process_start_time(pid) else {
                continue;
            };
            if end_run(pid, birth) {
                outcome.killed += 1;
            }
        }
    }
    if first {
        // The machine arm's reason is already a plain sentence; a person's
        // stop names who held the tests and why.
        let lead = if record.changed_by == fleet_incident::MACHINE_ORIGIN {
            format!(
                "{}. Running fleet tests end now (CI covers them) until it cools down. New workers pause too if it stays this busy.",
                record.reason
            )
        } else {
            format!(
                "Tests are held by {} ({}).",
                record.changed_by, record.reason
            )
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
/// expires after `ttl_secs` and end the running tests. `Ok(None)` when a
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
        "tests held first (generation {}, {} ended)",
        record.generation, outcome.killed
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::census::test_proc_row as row;

    /// A held fleet test run ends (SIGKILL, not SIGSTOP) within one
    /// reconcile pass, and the pid-anchored claims it held (`build:cargo`,
    /// a run slot) admit a waiting build at once. The selection matrix the
    /// hold kills on (root detection, subtree, spare) travels with it: one
    /// end-to-end pass over the surface, nothing unguarded.
    #[test]
    fn a_hold_ends_fleet_test_subtrees_and_frees_their_claims() {
        use std::os::unix::process::ExitStatusExt as _;

        // The selection matrix the kill runs on: root detection, whole
        // subtree, the spare set.
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

        let root = std::env::temp_dir().join(format!("fno-test-hold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("bin")).unwrap();
        // A real binary named `cargo` that naps. A shebang script will not
        // do: the process table shows the interpreter's argv (`/bin/sh
        // <path>/cargo test ...`), which `is_test_root` never matches.
        let shim = root.join("bin/cargo");
        let source = root.join("bin/shim.rs");
        std::fs::write(
            &source,
            "fn main() { std::thread::sleep(std::time::Duration::from_secs(30)); }\n",
        )
        .unwrap();
        let built = std::process::Command::new("rustc")
            .arg("-o")
            .arg(&shim)
            .arg(&source)
            .output()
            .unwrap();
        assert!(
            built.status.success(),
            "rustc failed: {}",
            String::from_utf8_lossy(&built.stderr)
        );
        let mut child = std::process::Command::new(&shim)
            .args(["test", "-p", "held-crate"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();

        let _home = crate::AgentsHomeEnvGuard::set(&root);
        let home = crate::paths::AgentsHome::from_env();
        let mut visible = false;
        for _ in 0..50 {
            let (table, _) = crate::census::process_table();
            if table
                .iter()
                .any(|r| r.pid == pid && is_test_root(&r.command))
            {
                visible = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(visible, "the shim never surfaced as a fleet test root");

        let mut entry = crate::state::RegistryEntry::default();
        entry.name = "held-run-worker".into();
        entry.status = crate::AgentStatus::Busy;
        entry.pid = Some(pid);
        entry.pid_start_time = crate::daemon::process_start_time(pid);
        entry.harness_session_id = Some("held-run-sess".into());
        crate::state::update_registry(&home.registry_json(), |r| r.entries.push(entry)).unwrap();

        crate::fleet_incident::write_transition_with_metadata(
            &fleet_incident::fleet_stop_path(&home),
            "stopped",
            Some("machine overloaded"),
            Some(fleet_incident::MACHINE_ORIGIN),
            vec!["tests".to_string()],
            fleet_incident::RecordMetadata {
                origin: Some(fleet_incident::MACHINE_ORIGIN.to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        // The run's slots, anchored to its pid exactly as the admission
        // doors write them.
        let holder = format!("cargo:{}:{}", pid, root.display());
        let opts = |reason: &'static str| crate::claims::AcquireOpts {
            pid: Some(pid),
            reason: Some(reason.into()),
            root: Some(root.clone()),
            ..Default::default()
        };
        assert!(matches!(
            crate::claims::acquire("build:cargo", &holder, opts("held build")),
            crate::claims::AcquireOutcome::Acquired(_)
        ));
        assert!(matches!(
            crate::claims::acquire("test:cargo-run:0", &holder, opts("held slot")),
            crate::claims::AcquireOutcome::Acquired(_)
        ));

        let outcome = reconcile(&home).unwrap();
        assert_eq!(outcome.paused, 0, "a hold must never pause: {outcome:?}");
        let status = child.wait().unwrap();
        // This pass or the machine's own watcher may have ended the run; the
        // contract under test is that a held run ends by SIGKILL and its
        // slots free, not which process delivered the signal.
        assert!(
            outcome.killed >= 1 || status.signal() == Some(libc::SIGKILL),
            "the hold must end the run: {outcome:?} status {status:?}"
        );

        // The reaped pid frees its no-TTL claims: a waiting build admits at
        // once instead of queueing behind the corpse.
        for key in ["build:cargo", "test:cargo-run:0"] {
            let mut next = opts("waiting build");
            next.pid = Some(std::process::id());
            assert!(
                matches!(
                    crate::claims::acquire(key, "cargo:next", next),
                    crate::claims::AcquireOutcome::Acquired(_)
                ),
                "{key} did not free within one tick"
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }
}
