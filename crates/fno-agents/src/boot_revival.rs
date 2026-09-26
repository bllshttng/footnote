//! Bring the fleet back after a reboot.
//!
//! On the first daemon start of a boot, every registry worker that was live
//! at the boot comes back without a tap: `claude agents --json --all` lists
//! its job as stopped or failed, so `claude respawn <job id>` restarts it.
//! A worker whose node is done, merged or superseded stays stopped. Kings go
//! first. A revival re-seats a row that already held a seat, so it never
//! asks the spawn gate. One receipt row per worker lands in
//! `<agents home>/boot-revival.json`, and that file's boot stamp is what
//! makes the pass run once per boot.

use std::collections::HashSet;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::claude_roster::ClaudeAgentsSnapshot;
use crate::paths::AgentsHome;
use crate::state::RegistryEntry;

const RECEIPT_FILE: &str = "boot-revival.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ReceiptRow {
    pub(crate) name: String,
    /// "revived", "skipped" or "failed".
    pub(crate) outcome: &'static str,
    pub(crate) reason: String,
}

impl ReceiptRow {
    fn new(name: &str, outcome: &'static str, reason: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            outcome,
            reason: reason.into(),
        }
    }
}

/// Plan the pass once, then revive off the caller's thread. Planning reads
/// the registry before the startup sweep rewrites a single status, so a row
/// still reads the way it did when the machine went down. A second daemon
/// start in the same boot finds the receipt and does nothing.
pub(crate) fn start(home: &AgentsHome) {
    let Some(boot) = boot_time() else {
        return;
    };
    let receipt = home.root().join(RECEIPT_FILE);
    if recorded_boot(&receipt) == Some(boot) {
        return;
    }
    let Ok(registry) = crate::state::load_registry(&home.registry_json()) else {
        return;
    };
    let listing = crate::claude_roster::read_all_agents_union();
    let closed = closed_nodes();
    let (kings, workers, mut rows) = plan(&registry.entries, &listing, &closed, boot);
    // Stamp the boot first: a daemon that dies mid-pass must not respawn the
    // fleet again on its next start. The tap is the fallback for a row the
    // pass never reached.
    write_receipt(&receipt, boot, &rows);
    let home = home.clone();
    std::thread::spawn(move || {
        for name in &kings {
            rows.push(revive(&home, name));
        }
        let handles: Vec<_> = workers
            .into_iter()
            .map(|name| {
                let home = home.clone();
                std::thread::spawn(move || revive(&home, &name))
            })
            .collect();
        for handle in handles {
            if let Ok(row) = handle.join() {
                rows.push(row);
            }
        }
        write_receipt(&receipt, boot, &rows);
    });
}

/// Split the registry into kings to revive, workers to revive, and the rows
/// skipped with their reason. A row counts only when it was live at the
/// boot: a live-ish status, or an exit stamped after the boot began.
pub(crate) fn plan(
    entries: &[RegistryEntry],
    listing: &ClaudeAgentsSnapshot,
    closed_nodes: &HashSet<String>,
    boot: u64,
) -> (Vec<String>, Vec<String>, Vec<ReceiptRow>) {
    let mut kings = Vec::new();
    let mut workers = Vec::new();
    let mut skipped = Vec::new();
    for e in entries {
        if e.harness_name() != "claude" || e.mux.is_some() {
            continue;
        }
        let exited_after_boot = e
            .exited_at
            .as_deref()
            .and_then(crate::state::rfc3339_like_to_secs)
            .is_some_and(|at| at >= boot);
        if !crate::daemon::is_non_terminal(e.status) && !exited_after_boot {
            continue;
        }
        let Some(sid) = e.harness_session_id.as_deref().filter(|s| s.len() >= 8) else {
            skipped.push(ReceiptRow::new(
                &e.name,
                "skipped",
                "no session id recorded",
            ));
            continue;
        };
        if let Some(node) = e.node.as_deref().filter(|n| closed_nodes.contains(*n)) {
            skipped.push(ReceiptRow::new(
                &e.name,
                "skipped",
                format!("node {node} is done, merged or superseded"),
            ));
            continue;
        }
        let job: String = sid.chars().take(8).collect();
        let state = listing.find(&job).and_then(|row| row.state.clone());
        match state.as_deref() {
            Some("stopped" | "failed") => {
                if e.crown_level.is_some() {
                    kings.push(e.name.clone());
                } else {
                    workers.push(e.name.clone());
                }
            }
            Some(other) => skipped.push(ReceiptRow::new(
                &e.name,
                "skipped",
                format!("claude agents lists job {job} as {other}"),
            )),
            None => skipped.push(ReceiptRow::new(
                &e.name,
                "skipped",
                format!("claude agents does not list job {job}"),
            )),
        }
    }
    (kings, workers, skipped)
}

/// One revival through the canonical re-entry plan: the listing lists the
/// job, so the plan is `claude respawn <job id>`.
fn revive(home: &AgentsHome, name: &str) -> ReceiptRow {
    let plan = match crate::reentry::resolve_reentry(
        &home.registry_json(),
        name,
        crate::reentry::ReentryTransition::Resume,
        None,
        None,
    ) {
        Ok(plan) => plan,
        Err(reason) => return ReceiptRow::new(name, "failed", reason),
    };
    match crate::resume_wake::run_and_confirm_respawn(&plan, name, "revive", "agent_revived", home)
    {
        0 => ReceiptRow::new(name, "revived", plan.argv.join(" ")),
        code => ReceiptRow::new(
            name,
            "failed",
            format!("{} exited {code}", plan.argv.join(" ")),
        ),
    }
}

/// The nodes a revival must leave alone: done, superseded, or merged.
fn closed_nodes() -> HashSet<String> {
    let rows =
        crate::graph_store::read_rows(&crate::graph_get::default_graph_path()).unwrap_or_default();
    rows.iter()
        .filter(|row| {
            crate::graph_store::is_terminal_entry(row)
                || row.get("merge_status").and_then(Value::as_str) == Some("merged")
        })
        .filter_map(|row| crate::graph_store::entry_id(row).map(str::to_string))
        .collect()
}

fn recorded_boot(receipt: &Path) -> Option<u64> {
    let raw = std::fs::read_to_string(receipt).ok()?;
    serde_json::from_str::<Value>(&raw)
        .ok()?
        .get("boot")
        .and_then(Value::as_u64)
}

fn write_receipt(receipt: &Path, boot: u64, rows: &[ReceiptRow]) {
    let body = serde_json::json!({ "boot": boot, "rows": rows });
    let tmp = receipt.with_extension("json.tmp");
    if std::fs::write(&tmp, body.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, receipt);
    }
}

/// This boot's start, in epoch seconds.
fn boot_time() -> Option<u64> {
    if cfg!(target_os = "linux") {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        return stat
            .lines()
            .find_map(|line| line.strip_prefix("btime "))
            .and_then(|n| n.trim().parse().ok());
    }
    let out = std::process::Command::new("sysctl")
        .args(["-n", "kern.boottime"])
        .output()
        .ok()?;
    parse_boottime(&String::from_utf8_lossy(&out.stdout))
}

/// `{ sec = 1790338000, usec = 123 } Thu Sep 24 ...` -> 1790338000.
fn parse_boottime(text: &str) -> Option<u64> {
    let rest = text.split("sec = ").nth(1)?;
    rest.split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_roster::ClaudeAgentRow;
    use crate::AgentStatus;

    const BOOT: u64 = 1_790_338_000;

    fn row(name: &str, sid: &str, status: AgentStatus) -> RegistryEntry {
        RegistryEntry {
            name: name.into(),
            harness: Some("claude".into()),
            harness_session_id: Some(sid.into()),
            status,
            ..Default::default()
        }
    }

    #[test]
    fn the_boot_pass_revives_stopped_and_failed_workers_kings_first() {
        let mut king = row("quill", "99473043-aaaa", AgentStatus::Live);
        king.crown_level = Some(1);
        let failed = row("kestrel", "11112222-bbbb", AgentStatus::Orphaned);
        // Exited by this boot's own daemon: it was live when the machine
        // went down.
        let mut exited_now = row("folio", "33334444-cccc", AgentStatus::Exited);
        exited_now.exited_at = Some("2026-09-26T02:00:00Z".into());
        // Exited long before the boot: a deliberate stop stays stopped.
        let mut stopped_before = row("old", "55556666-dddd", AgentStatus::Exited);
        stopped_before.exited_at = Some("2026-01-01T00:00:00Z".into());
        let mut done_node = row("shipped", "77778888-eeee", AgentStatus::Live);
        done_node.node = Some("x-done".into());
        let running = row("candor", "9999aaaa-ffff", AgentStatus::Live);
        let unlisted = row("warden", "49a80492-0000", AgentStatus::Live);
        let listing = ClaudeAgentsSnapshot::known(vec![
            ClaudeAgentRow::new("99473043", Some("stopped")),
            ClaudeAgentRow::new("11112222", Some("failed")),
            ClaudeAgentRow::new("33334444", Some("stopped")),
            ClaudeAgentRow::new("55556666", Some("stopped")),
            ClaudeAgentRow::new("77778888", Some("stopped")),
            ClaudeAgentRow::new("9999aaaa", Some("working")),
        ]);
        let closed = HashSet::from(["x-done".to_string()]);
        let boot = crate::state::rfc3339_like_to_secs("2026-09-26T01:00:00Z").unwrap();
        let (kings, workers, skipped) = plan(
            &[
                king,
                failed,
                exited_now,
                stopped_before,
                done_node,
                running,
                unlisted,
            ],
            &listing,
            &closed,
            boot,
        );
        assert_eq!(kings, vec!["quill"]);
        assert_eq!(workers, vec!["kestrel", "folio"]);
        let names: Vec<_> = skipped.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["shipped", "candor", "warden"], "{skipped:?}");
        assert!(skipped.iter().all(|r| r.outcome == "skipped"));
        assert!(skipped[0].reason.contains("x-done"), "{skipped:?}");
        assert!(skipped[1].reason.contains("working"), "{skipped:?}");
    }

    #[test]
    fn boottime_parses_the_sysctl_line() {
        assert_eq!(
            parse_boottime("{ sec = 1790338000, usec = 482731 } Thu Sep 24 10:00:00 2026\n"),
            Some(BOOT)
        );
        assert_eq!(parse_boottime(""), None);
    }

    #[test]
    fn the_receipt_stamp_makes_the_pass_run_once_per_boot() {
        let dir = tempfile::tempdir().unwrap();
        let receipt = dir.path().join(RECEIPT_FILE);
        assert_eq!(recorded_boot(&receipt), None);
        write_receipt(
            &receipt,
            BOOT,
            &[ReceiptRow::new(
                "quill",
                "revived",
                "claude respawn 99473043",
            )],
        );
        assert_eq!(recorded_boot(&receipt), Some(BOOT));
    }
}
