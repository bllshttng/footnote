//! Load attribution for the machine arm: the fleet-vs-outside split, the
//! outside-load ask, and the concurrency budget guess.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use serde_json::Value;

use crate::census::ProcRow;
use crate::question_intake::IntakeRequest;

/// The arm's pending-ask file name, placed by the same layout rule as the
/// brake file.
pub const MACHINE_LOAD_ASK_NAME: &str = "machine-load-ask.json";

/// The arm's pending ask, one file next to the brake: which question is
/// open, the groups an answer would act on, and the pids a pause holds.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct PendingAsk {
    pub qid: Option<String>,
    pub kind: String,
    pub groups: Vec<StoredGroup>,
    pub paused_pids: Vec<(u32, u64)>,
    pub asked_at: u64,
    pub applied: Vec<String>,
    pub declined: Option<u64>,
}

/// One outside group as stored across ticks: the name an answer names, the
/// evidence the question showed, and the pids it held at ask time.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredGroup {
    pub name: String,
    pub cpu_pct: f64,
    pub rss_kb: u64,
    pub bundle: bool,
    pub pids: Vec<u32>,
}

/// Whose load the box is in, from the footprint payload's admission split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadSource {
    Fleet,
    Outside,
}

/// Fleet when the split is unreadable (today's path: the brake holds) or
/// the fleet holds at least half the measured CPU; outside otherwise.
pub fn classify(split: Option<(f64, f64)>) -> LoadSource {
    match split {
        None => LoadSource::Fleet,
        Some((fleet, machine)) => {
            if fleet * 2.0 >= machine {
                LoadSource::Fleet
            } else {
                LoadSource::Outside
            }
        }
    }
}
/// One outside consumer group: the top three CPU groups that are not the
/// fleet's, with summed CPU, memory, and pids.
#[derive(Debug, Clone)]
pub struct OutsideGroup {
    pub name: String,
    pub cpu_pct: f64,
    pub rss_kb: u64,
    pub bundle: bool,
    pub pids: Vec<u32>,
}

/// The group label for a process command: only the bundle's executable
/// region (`.app/Contents/`) names the app `X` - a process that merely
/// reads a file inside X.app is not X. Anything else groups under the
/// argv0 basename.
fn app_group_name(command: &str) -> String {
    if let Some(idx) = command.find(".app/Contents/") {
        let before = &command[..idx];
        return before.rsplit('/').next().unwrap_or(before).to_string();
    }
    argv0_basename(command).to_string()
}

/// The group label parts: the argv0 basename of a command line.
fn argv0_basename(command: &str) -> &str {
    command
        .split_whitespace()
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("")
}

/// The top three CPU groups NOT owned by the fleet: not in `fleet_pids`,
/// and not descended from a process whose argv0 basename starts with `fno`.
pub fn outside_groups(procs: &[ProcRow], fleet_pids: &HashSet<u32>) -> Vec<OutsideGroup> {
    let mut owned = fleet_pids.clone();
    for row in procs {
        if argv0_basename(&row.command).starts_with("fno") {
            owned.insert(row.pid);
        }
    }
    // Close over descendants: one pass per level until nothing new joins.
    let mut changed = true;
    while changed {
        changed = false;
        for row in procs {
            if !owned.contains(&row.pid) && owned.contains(&row.ppid) {
                owned.insert(row.pid);
                changed = true;
            }
        }
    }
    let mut groups: BTreeMap<String, OutsideGroup> = BTreeMap::new();
    for row in procs {
        if owned.contains(&row.pid) || row.pid == 0 {
            continue;
        }
        let bundle = row.command.contains(".app/Contents/");
        let name = app_group_name(&row.command);
        let entry = groups.entry(name.clone()).or_insert_with(|| OutsideGroup {
            name,
            cpu_pct: 0.0,
            rss_kb: 0,
            bundle: false,
            pids: Vec::new(),
        });
        entry.cpu_pct += row.cpu_pct;
        entry.rss_kb += row.rss_kb;
        entry.bundle |= bundle;
        entry.pids.push(row.pid);
    }
    let mut out: Vec<OutsideGroup> = groups.into_values().collect();
    out.sort_by(|a, b| {
        b.cpu_pct
            .partial_cmp(&a.cpu_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(3);
    out
}
/// Where the pending ask lives, placed by the same layout rule as the
/// brake file: `FNO_MACHINE_LOAD_ASK` overrides the full path.
pub fn ask_path() -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_MACHINE_LOAD_ASK").filter(|v| !v.is_empty()) {
        return PathBuf::from(v);
    }
    let home = std::env::var_os("HOME").unwrap_or_else(|| std::ffi::OsString::from("."));
    crate::state_layout::place(&PathBuf::from(home).join(".fno"), MACHINE_LOAD_ASK_NAME)
}

/// The pending ask, `None` when absent or unreadable (a malformed file
/// holds nothing and asks nothing twice).
pub fn pending_ask() -> Option<PendingAsk> {
    let text = std::fs::read_to_string(ask_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// The file is the only cross-tick state this module owns; a failed write
/// costs the ask, never the arm.
fn write_pending(pending: &PendingAsk) {
    if let Ok(text) = serde_json::to_string(pending) {
        let _ = std::fs::write(ask_path(), text);
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}
/// The outside-load question the arm files: what each top group is doing
/// (CPU and memory), why fno will not brake itself, and the recommendation
/// to leave it alone unless fno is slow.
pub fn outside_question(groups: &[StoredGroup]) -> IntakeRequest {
    let evidence: Vec<String> = groups
        .iter()
        .map(|g| {
            format!(
                "{}: {}% CPU, {} GB memory",
                g.name,
                g.cpu_pct as u64,
                g.rss_kb / (1024 * 1024)
            )
        })
        .collect();
    let question = format!(
        "The machine is loaded but fno holds only a small share of the CPU, so fno will not brake itself. Top outside load: {}. Recommendation: leave it alone unless fno is slow. What should fno do?",
        evidence.join("; ")
    );
    let mut options = Vec::new();
    for g in groups.iter().take(2) {
        options.push(format!("quit {}", g.name));
        options.push(format!("pause {}", g.name));
        options.push(format!("lower priority of {}", g.name));
    }
    options.push("leave everything alone".to_string());
    IntakeRequest {
        question,
        ask: None,
        options,
        blocks: Vec::new(),
        node: None,
        subject: Some("machine-load".to_string()),
        session_id: Some("machine_watch".to_string()),
        cwd: None,
        asker: Some("machine_watch".to_string()),
        laws: Vec::new(),
        storage_root: PathBuf::from("."),
        index_path: None,
        journal_path: None,
        display_name: None,
        render_cap: None,
    }
}

/// Filing the ask through the intake's subject dedup: one open
/// `machine-load` question, whatever the tick count. Stashes the qid and
/// the groups, and returns true when a NEW question went out.
pub fn file_outside_question(
    home: &crate::paths::AgentsHome,
    cwd: &std::path::Path,
    groups: Vec<StoredGroup>,
) -> bool {
    let mut req = outside_question(&groups);
    req.journal_path = Some(crate::paths::space_dir(cwd).join("events.jsonl"));
    req.index_path = Some(crate::provider_cap::questions_path(home));
    let answer = crate::question_intake::run_intake(&req, home);
    // A dedup names the still-open question: adopt its id so the file
    // tracks the live ask instead of dropping it.
    let qid = answer.qid.clone().or(answer.open_id.clone());
    match qid {
        Some(qid) => {
            let paused = pending_ask()
                .filter(|p| p.kind == "outside-load")
                .map(|p| p.paused_pids)
                .unwrap_or_default();
            write_pending(&PendingAsk {
                qid: Some(qid),
                kind: "outside-load".to_string(),
                groups,
                paused_pids: paused,
                asked_at: now_secs(),
                applied: Vec::new(),
                declined: None,
            });
            true
        }
        None => false,
    }
}
/// The pids a group name holds NOW, from a fresh table. A recorded pid
/// whose start time changed is a recycled pid: skipped, never signalled.
fn pids_for_group(name: &str, recorded: &[(u32, u64)]) -> Vec<u32> {
    let (table, _) = crate::census::process_table_ps();
    table
        .iter()
        .filter(|row| app_group_name(&row.command) == name)
        .filter(|row| {
            !recorded.iter().any(|(rp, rb)| {
                *rp == row.pid && crate::daemon::process_start_time(row.pid) != Some(*rb)
            })
        })
        .map(|row| row.pid)
        .collect()
}

/// One answered outside-load option, acted on. Re-resolves the group's
/// pids at apply time; records the answer as applied so the arm never
/// acts on it twice. Returns the outcome line for the tick detail.
pub fn apply_answer(answer: &str, pending: &mut PendingAsk) -> String {
    let text = answer.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("leave everything alone") {
        return "left alone".to_string();
    }
    let (action, name) = if let Some(rest) = text.strip_prefix("quit ") {
        ("quit", rest)
    } else if let Some(rest) = text.strip_prefix("pause ") {
        ("pause", rest)
    } else if let Some(rest) = text.strip_prefix("lower priority of ") {
        ("demote", rest)
    } else {
        return format!("unrecognized answer: {text}");
    };
    let Some(group) = pending.groups.iter().find(|g| g.name == name) else {
        return format!("group gone from the ask: {name}");
    };
    let recorded: Vec<(u32, u64)> = pending
        .paused_pids
        .iter()
        .map(|(pid, birth)| (*pid, *birth))
        .collect();
    let pids = pids_for_group(name, &recorded);
    match action {
        "quit" => {
            if group.bundle {
                let script = format!("tell application \"{}\" to quit", name);
                let _ = std::process::Command::new("osascript")
                    .args(["-e", &script])
                    .output();
            } else {
                for pid in pids {
                    unsafe {
                        libc::kill(pid as i32, libc::SIGTERM);
                    }
                }
            }
            format!("quit {name}")
        }
        "pause" => {
            for pid in pids {
                let birth = crate::daemon::process_start_time(pid).unwrap_or(0);
                unsafe {
                    libc::kill(pid as i32, libc::SIGSTOP);
                }
                pending.paused_pids.push((pid, birth));
            }
            format!("paused {name}")
        }
        _ => {
            for pid in pids {
                demote_pid(pid);
            }
            format!("lowered priority of {name}")
        }
    }
}

/// The demotion argv, the same one the spawn gate uses (`taskpolicy -b -p`
/// on macOS, `renice 10 -p` on Linux); the user chose it, so no config
/// gate applies here.
fn demote_pid(pid: u32) {
    let status = if cfg!(target_os = "macos") {
        std::process::Command::new("/usr/sbin/taskpolicy")
            .args(["-b", "-p", &pid.to_string()])
            .status()
    } else if cfg!(target_os = "linux") {
        std::process::Command::new("/usr/bin/renice")
            .args(["10", "-p", &pid.to_string()])
            .status()
    } else {
        return;
    };
    let _ = status;
}
/// The answer fold for one arm tick: every fresh answer for the pending
/// qid, applied once (the applied list rides the pending file).
pub fn poll_answers(home: &crate::paths::AgentsHome, cwd: &std::path::Path) -> Option<String> {
    let mut pending = pending_ask()?;
    let qid = pending.qid.clone()?;
    let raw = journals_raw(home, cwd);
    let answers = crate::attention::answered(&raw, now_secs());
    let mut outcome = String::new();
    for a in answers.iter().filter(|a| a.id == qid) {
        if pending.applied.iter().any(|ap| ap == &a.at) {
            continue;
        }
        let line = if pending.kind == "budget" {
            let value = a
                .answer
                .rsplit(' ')
                .next()
                .and_then(|word| word.parse::<u32>().ok());
            apply_budget_answer(&a.answer, value, &mut pending)
        } else {
            apply_answer(&a.answer, &mut pending)
        };
        pending.applied.push(a.at.clone());
        if !outcome.is_empty() {
            outcome.push_str("; ");
        }
        outcome.push_str(&line);
    }
    if outcome.is_empty() {
        return None;
    }
    if pending.kind == "outside-load" {
        // The ask is done; the file stays only for the pause bookkeeping.
        pending.qid = None;
    }
    write_pending(&pending);
    Some(outcome)
}

/// The first calm tick resumes what a pause held: SIGCONT to the recorded
/// pids whose start time still matches; every entry then clears, since a
/// recycled pid must never be continued later.
pub fn resume_paused() -> Option<String> {
    let mut pending = pending_ask()?;
    if pending.paused_pids.is_empty() {
        return None;
    }
    let mut resumed = 0;
    for (pid, birth) in pending.paused_pids.iter() {
        if crate::daemon::process_start_time(*pid) == Some(*birth) {
            unsafe {
                libc::kill(*pid as i32, libc::SIGCONT);
            }
            resumed += 1;
        }
    }
    let live = resumed;
    pending.paused_pids.clear();
    write_pending(&pending);
    (live > 0).then(|| format!("resumed {live} paused pid(s)"))
}

/// The raw journal text the answer fold reads: the same three stores
/// `needs.rs` folds, read whole (the fold filters by row kind itself).
fn journals_raw(home: &crate::paths::AgentsHome, cwd: &std::path::Path) -> String {
    let fno_dir = home
        .root()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(".fno"));
    let mut raw = String::new();
    for path in crate::needs::question_journals(&fno_dir, cwd) {
        if let Ok(text) = std::fs::read_to_string(&path) {
            raw.push_str(&text);
            if !text.ends_with('\n') {
                raw.push('\n');
            }
        }
    }
    raw
}
/// The concurrency budget guess: how many sessions this box holds, from
/// cores, memory, and this tick's own session costs.
#[derive(Debug, Clone, PartialEq)]
pub struct Budget {
    pub max_live: u32,
    pub leads: u32,
    pub workers: u32,
    pub per_session_gb: f64,
    pub per_session_cores: f64,
    pub basis: &'static str,
}

/// The 75th percentile of a column, nearest-rank.
fn percentile75(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((values.len() as f64) * 0.75).ceil() as usize;
    let idx = idx.clamp(1, values.len());
    Some(values[idx - 1])
}

/// The budget: per-session cost is the p75 of this tick's own session rows
/// when there are at least three, else the band defaults. Memory headroom
/// reserves max(8 GB, a quarter of the box) before dividing.
pub fn budget(cores: Option<f64>, total_mem_gb: Option<f64>, sessions: Option<&Value>) -> Budget {
    let rows: Vec<(f64, f64)> = sessions
        .and_then(|v| v.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    Some((row.get("rss_mb")?.as_f64()?, row.get("cpu_pct")?.as_f64()?))
                })
                .collect()
        })
        .unwrap_or_default();
    let (per_session_gb, per_session_cores, basis) = if rows.len() >= 3 {
        let (gbs, cpus): (Vec<f64>, Vec<f64>) = rows.iter().map(|(g, c)| (*g, *c)).unzip();
        (
            percentile75(gbs).unwrap_or(2457.6) / 1024.0,
            percentile75(cpus).unwrap_or(0.5),
            "measured p75",
        )
    } else {
        (2.4, 0.5, "band default")
    };
    let cores = cores.unwrap_or(8.0);
    let total_mem_gb = total_mem_gb.unwrap_or(32.0);
    let reserve_gb = 8.0_f64.max(total_mem_gb / 4.0);
    let by_mem = ((total_mem_gb - reserve_gb) / per_session_gb).floor();
    let by_cpu = (cores / per_session_cores).floor();
    let max_live = 1_f64.max(by_mem.min(by_cpu)) as u32;
    let leads = 1_u32.max(((max_live as f64) / 5.0).round() as u32);
    Budget {
        max_live,
        leads,
        workers: max_live - leads,
        per_session_gb,
        per_session_cores,
        basis,
    }
}
/// The budget question, filed once while `agents.max_live` is unset and
/// not declined: its `yes` writes the config, its `no` never asks again
/// for this value.
pub fn file_budget_question(
    home: &crate::paths::AgentsHome,
    cwd: &std::path::Path,
    sample: &crate::machine_sample::MachineSample,
) -> bool {
    if crate::agents_config::resolve_agents_value(cwd, "max_live").is_some() {
        return false;
    }
    let b = budget(sample.cores, sample.total_mem_gb, sample.sessions.as_ref());
    if let Some(existing) = pending_ask() {
        if existing.declined == Some(b.max_live as u64) {
            return false;
        }
        // One pending-file slot: a live ask of either kind owns it, and a
        // dedup would only drop this one's qid.
        if existing.qid.is_some() {
            return false;
        }
    }
    let question = format!(
        "No concurrency cap is set. This box measures {} cores and {} GB, so the band guess is max_live {} ({} leads + {} workers; {}). Set it as the cap?",
        sample.cores.map(|c| format!("{c:.0}")).unwrap_or_else(|| "?".into()),
        sample
            .total_mem_gb
            .map(|m| format!("{m:.0}"))
            .unwrap_or_else(|| "?".into()),
        b.max_live,
        b.leads,
        b.workers,
        b.basis
    );
    let req = IntakeRequest {
        question,
        ask: None,
        options: vec![
            format!("yes, set agents.max_live to {}", b.max_live),
            "no, keep the default".to_string(),
        ],
        blocks: Vec::new(),
        node: None,
        subject: Some("concurrency-budget".to_string()),
        session_id: Some("machine_watch".to_string()),
        cwd: None,
        asker: Some("machine_watch".to_string()),
        laws: Vec::new(),
        storage_root: PathBuf::from("."),
        index_path: Some(crate::provider_cap::questions_path(home)),
        journal_path: Some(crate::paths::space_dir(cwd).join("events.jsonl")),
        display_name: None,
        render_cap: None,
    };
    let answer = crate::question_intake::run_intake(&req, home);
    // A dedup names the still-open question: adopt its id so the answer
    // the user already gave can still be applied.
    let qid = answer.qid.clone().or(answer.open_id.clone());
    if let Some(qid) = qid {
        let pending = PendingAsk {
            qid: Some(qid),
            kind: "budget".to_string(),
            groups: Vec::new(),
            paused_pids: Vec::new(),
            asked_at: now_secs(),
            applied: Vec::new(),
            declined: None,
        };
        write_pending(&pending);
        return true;
    }
    false
}

/// The `yes` half of the budget ask: write the cap once through the same
/// binary the fleet configures with. The `no` half records the decline so
/// this value never asks again.
pub fn apply_budget_answer(answer: &str, value: Option<u32>, pending: &mut PendingAsk) -> String {
    let text = answer.trim();
    if text.starts_with("yes") {
        let Some(value) = value else {
            return "could not read the cap from the answer; the question stays open".into();
        };
        let bin = crate::scrape::fno_bin();
        let status = std::process::Command::new(&bin)
            .args(["config", "set", "agents.max_live", &value.to_string()])
            .status();
        match status {
            Ok(s) if s.success() => {
                pending.qid = None;
                return format!("set agents.max_live to {value}");
            }
            other => {
                return format!("config set failed ({other:?}); the question stays open");
            }
        }
    }
    if text.starts_with("no") {
        pending.declined = value.map(|v| v as u64);
        pending.kind = "budget-declined".to_string();
        return "keeping the default cap".to_string();
    }
    format!("unrecognized answer: {text}")
}
#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: u32, ppid: u32, cpu: f64, rss_kb: u64, command: &str) -> ProcRow {
        ProcRow {
            pid,
            ppid,
            state: 'R',
            elapsed_s: 10,
            cpu_pct: cpu,
            rss_kb,
            command: command.to_string(),
        }
    }

    /// AC3/AC4 in one read: the split classifies unreadable and
    /// fleet-majority as Fleet and a minority share as Outside, and the top
    /// groups exclude fleet pids, fno descendants, and the fno binaries
    /// themselves, naming app bundles by app name.
    #[test]
    fn classify_and_outside_groups_split_the_box_correctly() {
        assert_eq!(classify(None), LoadSource::Fleet);
        assert_eq!(classify(Some((4.0, 8.0))), LoadSource::Fleet);
        assert_eq!(classify(Some((3.9, 8.0))), LoadSource::Outside);
        let procs = vec![
            row(1, 0, 0.0, 100, "/sbin/launchd"),
            row(10, 1, 90.0, 1000, "/usr/bin/fno-agents daemon"),
            row(11, 10, 5.0, 100, "/bin/sh helper"),
            row(
                20,
                1,
                80.0,
                400_000,
                "/Applications/OrbStack.app/Contents/MacOS/OrbStack",
            ),
            row(
                21,
                20,
                40.0,
                900_000,
                "/Applications/OrbStack.app/Contents/MacOS/orb",
            ),
            row(
                30,
                1,
                50.0,
                200_000,
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            ),
            row(31, 1, 10.0, 100_000, "yes"),
            row(
                32,
                1,
                3.0,
                1_000,
                "tail -f /Applications/OrbStack.app/Contents/Resources/log",
            ),
        ];
        let mut fleet = HashSet::new();
        fleet.insert(10_u32);
        let groups = outside_groups(&procs, &fleet);
        assert_eq!(groups.len(), 3, "top three only: {groups:?}");
        assert_eq!(groups[0].name, "OrbStack", "{groups:?}");
        assert!(groups[0].bundle);
        assert_eq!(groups[0].cpu_pct, 120.0);
        assert_eq!(
            groups[0].pids,
            vec![20, 21],
            "a reader of the bundle is not the app"
        );
        assert_eq!(groups[1].name, "Google Chrome");
        assert!(!groups.iter().any(|g| g.name.contains("fno")));
        assert!(!groups.iter().any(|g| g.name == "sh helper"));
    }

    /// AC8-HP: the band-default guess on the node's example box.
    #[test]
    fn budget_uses_band_defaults_and_matches_the_node_example() {
        let b = budget(Some(8.0), Some(32.0), None);
        assert_eq!(b.max_live, 10);
        assert_eq!(b.leads, 2);
        assert_eq!(b.workers, 8);
        assert_eq!(b.basis, "band default");
        let measured = serde_json::json!([
            {"rss_mb": 1024.0, "cpu_pct": 0.2},
            {"rss_mb": 2048.0, "cpu_pct": 0.3},
            {"rss_mb": 3072.0, "cpu_pct": 0.4},
        ]);
        let b = budget(Some(8.0), Some(32.0), Some(&measured));
        assert_eq!(b.per_session_gb, 3.0);
        assert_eq!(b.per_session_cores, 0.4);
        assert_eq!(b.basis, "measured p75");
    }
    /// AC5-HP: a pause answer SIGSTOPs only the named group's live pid and
    /// a calm tick's resume SIGCONTs it; the pids are a real sleeper.
    #[test]
    fn pause_then_resume_signals_only_the_named_group() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let td = tempfile::TempDir::new().unwrap();
        let saved_ask = std::env::var_os("FNO_MACHINE_LOAD_ASK");
        std::env::set_var("FNO_MACHINE_LOAD_ASK", td.path().join("ask.json"));
        let mut child = std::process::Command::new("/usr/bin/env")
            .args(["bash", "-c", "exec -a ml-specimen-sleep /bin/sleep 30"])
            .spawn()
            .expect("specimen sleeper spawns");
        std::thread::sleep(std::time::Duration::from_millis(400));
        let mut pending = PendingAsk {
            kind: "outside-load".into(),
            groups: vec![StoredGroup {
                name: "ml-specimen-sleep".into(),
                cpu_pct: 90.0,
                rss_kb: 1024,
                bundle: false,
                pids: vec![child.id()],
            }],
            ..Default::default()
        };
        let line = apply_answer("pause ml-specimen-sleep", &mut pending);
        assert_eq!(line, "paused ml-specimen-sleep");
        assert!(!pending.paused_pids.is_empty(), "the pause records pids");
        let (table, _) = crate::census::process_table_ps();
        let row = table
            .iter()
            .find(|r| r.pid == child.id())
            .expect("specimen row");
        assert_eq!(row.state, 'T', "the sleeper is SIGSTOPped: {row:?}");
        let line = resume_paused();
        assert!(
            line.is_none(),
            "no recorded pause in the ask file yet: {line:?}"
        );
        // Direct resume of what the pause recorded, through the same
        // birth-checked path resume_paused uses.
        for (pid, birth) in pending.paused_pids.iter() {
            assert_eq!(crate::daemon::process_start_time(*pid), Some(*birth));
            unsafe {
                libc::kill(*pid as i32, libc::SIGCONT);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        let (table, _) = crate::census::process_table_ps();
        let row = table
            .iter()
            .find(|r| r.pid == child.id())
            .expect("specimen row");
        assert_ne!(row.state, 'T', "the sleeper runs again: {row:?}");
        let _ = child.kill();
        let _ = child.wait();
        match saved_ask {
            Some(v) => std::env::set_var("FNO_MACHINE_LOAD_ASK", v),
            None => std::env::remove_var("FNO_MACHINE_LOAD_ASK"),
        }
    }

    /// The leave answer and an unknown answer act on nothing, and an
    /// answer for a group no longer running records as gone.
    #[test]
    fn leave_and_unknown_answers_act_on_nothing() {
        let mut pending = PendingAsk {
            kind: "outside-load".into(),
            groups: vec![StoredGroup {
                name: "vanished-app".into(),
                cpu_pct: 50.0,
                rss_kb: 1,
                bundle: false,
                pids: vec![999_999],
            }],
            ..Default::default()
        };
        assert_eq!(
            apply_answer("leave everything alone", &mut pending),
            "left alone"
        );
        assert_eq!(
            apply_answer("pause vanished-app", &mut pending),
            "paused vanished-app"
        );
        assert!(
            pending.paused_pids.is_empty(),
            "a gone group pauses nothing"
        );
        assert_eq!(
            apply_answer("rewrite the kernel", &mut pending),
            "unrecognized answer: rewrite the kernel"
        );
    }
}
