//! `fno-agents test-run builds`, which `fno doctor builds` runs: every cargo
//! job the admission doors know, in one table. A row names the cargo pid, its
//! state (running or waiting), its kind (check, build or test, from the cargo
//! argv), its lane, how long it has run or waited, and the checkout with the
//! node and session from that checkout's target manifest. It reads the
//! admission claims, the waiter markers and the lane dirs. It adds no event
//! stream of its own.

use std::collections::BTreeMap;
use std::path::Path;

/// The lane dirs beside the build door's lockfile, best first, with the name
/// the readout prints.
const BUILD_LANES: [(&str, &str); 5] = [
    ("priority", "priority"),
    ("repair", "repair"),
    ("prepush", "prepush"),
    ("queue", "normal"),
    ("full", "full"),
];

struct Job {
    cargo_pid: u32,
    worktree: String,
    slots: Vec<String>,
    since_ms: i64,
    waiting: Option<String>,
}

pub(crate) fn run(args: &[String]) -> i32 {
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            "-h" | "--help" => {
                println!("usage: fno doctor builds [--json]");
                return 0;
            }
            other => {
                eprintln!("fno doctor builds: unrecognized argument: {other}. usage: fno doctor builds [--json]");
                return 2;
            }
        }
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let now = crate::claims::now_ms();
    let load = crate::machine_sample::load_average().map(|(one, _, _)| one);
    let cap = crate::test_run::build_slot_cap(load);
    let mut keys: Vec<String> = crate::test_run::BUILD_SLOT_KEYS
        .iter()
        .map(|k| k.to_string())
        .collect();
    keys.extend(crate::test_run::cargo_slot_keys(&cwd));

    let mut jobs = BTreeMap::<u32, Job>::new();
    for key in &keys {
        let (state, rec) = crate::claims::status(key, None);
        if !matches!(
            state,
            crate::claims::ClaimState::Live | crate::claims::ClaimState::Suspect
        ) {
            continue;
        }
        let Some(rec) = rec else { continue };
        let Some((worktree, cargo_pid)) = parse_holder(&rec.holder) else {
            continue;
        };
        let job = jobs.entry(cargo_pid).or_insert_with(|| Job {
            cargo_pid,
            worktree,
            slots: Vec::new(),
            since_ms: rec.acquired_at,
            waiting: None,
        });
        job.slots.push(key.clone());
        job.since_ms = job.since_ms.min(rec.acquired_at);
    }
    for (wait, lane, worktree) in waiters() {
        let job = jobs.entry(wait.cargo_pid).or_insert_with(|| Job {
            cargo_pid: wait.cargo_pid,
            worktree: String::new(),
            slots: Vec::new(),
            since_ms: wait.since_ms,
            waiting: None,
        });
        job.since_ms = wait.since_ms;
        job.waiting = Some(lane);
        if job.worktree.is_empty() {
            job.worktree = worktree;
        }
    }

    let commands: std::collections::HashMap<u32, String> = crate::census::process_table()
        .0
        .into_iter()
        .map(|row| (row.pid, row.command))
        .collect();
    let queued: Vec<(&str, usize)> =
        crate::claims::claim_path(crate::test_run::BUILD_SLOT_KEYS[0], None)
            .map(|lock| {
                BUILD_LANES
                    .iter()
                    .map(|(dir, name)| {
                        (
                            *name,
                            crate::claim_queue::depth(&crate::claim_queue::lane_dir_for(
                                &lock, dir,
                            )),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();

    let rows: Vec<serde_json::Value> = jobs
        .values()
        .map(|job| {
            let (node, session) = manifest_ids(&job.worktree);
            let state = if job.waiting.is_some() {
                "waiting"
            } else {
                "running"
            };
            let kind = commands
                .get(&job.cargo_pid)
                .map_or("gone", |command| cargo_kind(command));
            serde_json::json!({
                "cargo_pid": job.cargo_pid,
                "state": state,
                "kind": kind,
                "lane": job.waiting,
                "slots": job.slots,
                "secs": ((now - job.since_ms) / 1000).max(0),
                "worktree": job.worktree,
                "node": node,
                "session": session,
            })
        })
        .collect();

    if json {
        let lanes: BTreeMap<String, usize> = queued
            .iter()
            .map(|(name, n)| ((*name).to_string(), *n))
            .collect();
        let out = serde_json::json!({
            "load": load,
            "compile_slots": cap,
            "queued": lanes,
            "jobs": rows,
        });
        println!("{out}");
        return 0;
    }
    println!(
        "compile slots for agents: {cap} at load {} (3 under 30, 2 to 60, 1 to 150, user only above)",
        load.map_or("unreadable".to_string(), |l| format!("{l:.0}"))
    );
    if !queued.is_empty() {
        let lanes = queued
            .iter()
            .map(|(name, n)| format!("{name} {n}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!("queued at the build door: {lanes}");
    }
    if rows.is_empty() {
        println!("no cargo job holds or waits at the admission doors");
        return 0;
    }
    println!(
        "{:<8} {:<8} {:<7} {:<8} {:>7}  {:<10} {:<10} CHECKOUT",
        "PID", "STATE", "KIND", "LANE", "TIME", "NODE", "SESSION"
    );
    for row in &rows {
        let text = |field: &str| row[field].as_str().unwrap_or("-").to_string();
        let session = text("session");
        println!(
            "{:<8} {:<8} {:<7} {:<8} {:>7}  {:<10} {:<10} {}",
            row["cargo_pid"].as_u64().unwrap_or(0),
            text("state"),
            text("kind"),
            text("lane"),
            duration(row["secs"].as_i64().unwrap_or(0)),
            text("node"),
            session.get(..8).unwrap_or(session.as_str()),
            text("worktree"),
        );
    }
    0
}

/// `cargo:<worktree>:<pid>`, the holder string both cargo doors write.
fn parse_holder(holder: &str) -> Option<(String, u32)> {
    let (prefix, pid) = holder.rsplit_once(':')?;
    let worktree = prefix.strip_prefix("cargo:")?;
    Some((worktree.to_string(), pid.parse().ok()?))
}

/// The live waiter markers, each with the lane it waits in and its checkout.
fn waiters() -> Vec<(crate::test_run::BuildWait, String, String)> {
    let Some(dir) = crate::claims::build_waiters_dir() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let Ok(raw) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let Some(worktree) = value["worktree"].as_str() else {
            continue;
        };
        if let Some(wait) = crate::test_run::live_waiter(&dir, Path::new(worktree)) {
            let lane = value["lane"].as_str().unwrap_or("normal").to_string();
            out.push((wait, lane, worktree.to_string()));
        }
    }
    out
}

/// The node and session a checkout's target manifest names, if it has one.
fn manifest_ids(worktree: &str) -> (Option<String>, Option<String>) {
    if worktree.is_empty() {
        return (None, None);
    }
    let Some(space) = crate::paths::worktree_space_dir_opt(Path::new(worktree)) else {
        return (None, None);
    };
    let Ok(manifest) = std::fs::read_to_string(space.join("target-state.md")) else {
        return (None, None);
    };
    let field = |name: &str| {
        let prefix = format!("{name}:");
        manifest.lines().find_map(|line| {
            line.trim()
                .strip_prefix(&prefix)
                .map(|v| v.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
                .filter(|v| !v.is_empty() && v != "null")
        })
    };
    (
        field("graph_node_id"),
        field("harness_session_id").or_else(|| field("session_id")),
    )
}

/// Check, build or test, from the cargo argv: the first word after the
/// program that is not a `+toolchain` pin or a flag.
fn cargo_kind(command: &str) -> &'static str {
    let sub = command
        .split_whitespace()
        .skip(1)
        .find(|w| !w.starts_with('+') && !w.starts_with('-'));
    match sub {
        Some("test" | "t" | "nextest" | "bench") => "test",
        Some("check" | "c" | "clippy") => "check",
        Some(_) => "build",
        None => "unknown",
    }
}

fn duration(secs: i64) -> String {
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_view_reads_holder_kind_and_duration() {
        assert_eq!(
            parse_holder("cargo:/w/x-b792:4242"),
            Some(("/w/x-b792".to_string(), 4242))
        );
        assert_eq!(parse_holder("worktree:/w"), None);
        assert_eq!(cargo_kind("/opt/cargo +1.94.1 test -p fno-agents"), "test");
        assert_eq!(cargo_kind("cargo check --workspace"), "check");
        assert_eq!(cargo_kind("cargo build --release"), "build");
        assert_eq!(cargo_kind("cargo"), "unknown");
        assert_eq!(duration(4), "4s");
        assert_eq!(duration(250), "4m10s");
        assert_eq!(duration(3725), "1h02m");
    }
}
