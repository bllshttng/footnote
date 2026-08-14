//! Roster-progress sidecar (x-cdc7 SECOND HALF): durable per-row git evidence
//! beside process liveness, so a live-looking worker with zero commits no
//! longer renders the same as a healthy one.
//!
//! Measured 2026-08-14: a roster row read `live` while its provider was
//! exhausted and it held zero commits - process liveness and durable
//! progress had silently disagreed, and nothing in the roster surfaced it.
//! `is_stale_despite_liveness` is the classifier that closes that gap.
//!
//! Deliberately a SIDECAR file (`roster-progress.json`, keyed by row name;
//! see [`crate::paths::AgentsHome::roster_progress_json`]) rather than new
//! `RegistryEntry` fields: ~20 existing literal constructions of that struct
//! across the crate would each need a matching field for data no reader
//! needs schema-version protection on (a reader unaware of this sidecar
//! simply sees no progress row, exactly like today). Stays OUT of
//! `crates/fno/src/agents_view.rs` (the t-x9de7 collision seam, live there
//! adding a tri-state `Liveness` enum): this module only computes and stores
//! the data; a later change reads and renders it, joined the same way the
//! registry itself is joined - by row name / cwd.

use crate::AgentStatus;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// One row's durable progress evidence, computed from its worktree's git
/// state - never from process liveness.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RosterProgress {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_commit_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_commit_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_ahead_of_origin: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    /// When this row's progress was last refreshed (RFC3339 UTC).
    #[serde(default)]
    pub computed_at: String,
}

/// The on-disk sidecar: every tracked row's latest progress, keyed by name
/// (the same join key `RegistryEntry.name` carries).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RosterProgressStore {
    #[serde(default)]
    pub rows: BTreeMap<String, RosterProgress>,
}

/// Compute one row's progress from its worktree. Git-only except for a
/// bounded `gh` call: `git log`/`rev-list` never touch the network, and `gh
/// pr view` fires only when an upstream was found, so a row still working
/// locally never pays for (or rate-limits against) a GitHub call. Reuses
/// `finalize`'s `git_capture`/`gh_pr_ref` rather than a third shelling
/// implementation of the same two operations.
pub fn compute(cwd: &Path, now: &str) -> RosterProgress {
    let mut p = RosterProgress {
        computed_at: now.to_string(),
        ..Default::default()
    };
    if !cwd.join(".git").exists() {
        return p; // not a git worktree at all
    }
    if let Some(raw) = crate::finalize::git_capture(cwd, &["log", "-1", "--format=%H%x1f%cI"]) {
        let mut parts = raw.splitn(2, '\u{1f}');
        p.last_commit_sha = parts.next().map(str::to_string).filter(|s| !s.is_empty());
        p.last_commit_at = parts.next().map(str::to_string);
    }
    if let Some(upstream) = crate::finalize::git_capture(
        cwd,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    ) {
        if !upstream.is_empty() {
            if let Some(count) = crate::finalize::git_capture(
                cwd,
                &["rev-list", "--count", &format!("{upstream}..HEAD")],
            ) {
                p.branch_ahead_of_origin = count.trim().parse().ok();
            }
            // Only ask GitHub when there is something to ask about: a branch
            // with no upstream cannot have an open PR, and skipping the call
            // keeps this bounded to pushed branches.
            if let Some((number, _url)) = crate::finalize::gh_pr_ref(cwd) {
                p.pr_number = Some(number);
            }
        }
    }
    p
}

/// Mirrors the reconcile sweep's own "still expected to be alive" set in
/// `daemon.rs::plan_reconcile` (Restarting/Failed/terminal states are
/// intentionally excluded there too - their lifecycle belongs to the restart
/// supervisor or is already over, not to this staleness read).
fn is_live_ish(status: AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Live
            | AgentStatus::Ready
            | AgentStatus::Idle
            | AgentStatus::Busy
            | AgentStatus::Spawning
    )
}

/// A live-looking row with zero commits well past a reasonable grace window
/// is exactly the row a king must be able to see: process liveness and
/// durable progress can now disagree, and today they render identically.
/// Pure so it is testable without a registry file or a real clock.
pub fn is_stale_despite_liveness(
    status: AgentStatus,
    progress: Option<&RosterProgress>,
    created_at_secs: Option<u64>,
    now_secs: u64,
    grace_secs: u64,
) -> bool {
    if !is_live_ish(status) {
        return false;
    }
    let has_commit = progress.is_some_and(|p| p.last_commit_sha.is_some());
    if has_commit {
        return false;
    }
    match created_at_secs {
        Some(created) => now_secs.saturating_sub(created) > grace_secs,
        // No reliable clock -> fail toward "not flagged", never a false alarm.
        None => false,
    }
}

/// Load the sidecar; a missing/corrupt file reads as empty (never an error -
/// this is best-effort evidence, not a gate anything blocks on).
pub fn load(path: &Path) -> RosterProgressStore {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Refresh one row's progress and write the sidecar back atomically
/// (tempfile + rename, same publish discipline as `state::write_json_atomic`).
/// Best-effort by design: the reconcile sweep treats a failure here as
/// non-load-bearing, same fatality as its other per-row probes.
pub fn refresh_row(path: &Path, name: &str, cwd: &Path, now: &str) -> std::io::Result<()> {
    let mut store = load(path);
    store.rows.insert(name.to_string(), compute(cwd, now));
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(&store)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git_fixture(dir: &Path) {
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .status()
                .unwrap()
        };
        run(&["init", "-q", "."]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
    }

    fn commit(dir: &Path, name: &str, msg: &str) {
        std::fs::write(dir.join(name), msg).unwrap();
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-q", "-m", msg])
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap();
    }

    #[test]
    fn compute_reads_the_real_head_of_a_git_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git_fixture(d);
        commit(d, "a.txt", "first");

        let p = compute(d, "2026-08-14T12:00:00Z");

        assert!(p.last_commit_sha.is_some());
        assert!(p.last_commit_at.is_some());
        assert_eq!(p.branch_ahead_of_origin, None, "no upstream configured");
        assert_eq!(p.pr_number, None, "no upstream -> never asks gh");
        assert_eq!(p.computed_at, "2026-08-14T12:00:00Z");
    }

    #[test]
    fn compute_is_empty_outside_a_git_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let p = compute(tmp.path(), "2026-08-14T12:00:00Z");
        assert_eq!(p.last_commit_sha, None);
        assert_eq!(p.last_commit_at, None);
    }

    #[test]
    fn refresh_and_load_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git_fixture(&repo);
        commit(&repo, "a.txt", "first");
        let sidecar = tmp.path().join("roster-progress.json");

        refresh_row(&sidecar, "worker-a", &repo, "2026-08-14T12:00:00Z").unwrap();
        let store = load(&sidecar);

        let row = store.rows.get("worker-a").expect("row present");
        assert!(row.last_commit_sha.is_some());
        assert_eq!(row.computed_at, "2026-08-14T12:00:00Z");
    }

    #[test]
    fn load_of_missing_file_is_empty_not_an_error() {
        let store = load(Path::new("/nonexistent/does-not-exist.json"));
        assert!(store.rows.is_empty());
    }

    // ── is_stale_despite_liveness: the "does not render as healthy" gate ───

    #[test]
    fn live_process_zero_commits_past_grace_is_not_healthy() {
        // The measured near-miss: a roster row reading live with zero
        // commits an hour in. Assert it does NOT classify as healthy.
        let stale =
            is_stale_despite_liveness(AgentStatus::Live, None, Some(1_000), 1_000 + 3_601, 3_600);
        assert!(stale, "a live row with zero commits past grace must flag");
    }

    #[test]
    fn live_process_zero_commits_within_grace_is_not_flagged() {
        let ok =
            is_stale_despite_liveness(AgentStatus::Live, None, Some(1_000), 1_000 + 600, 3_600);
        assert!(!ok, "still inside the grace window");
    }

    #[test]
    fn a_real_commit_is_never_flagged_regardless_of_age() {
        let progress = RosterProgress {
            last_commit_sha: Some("abc123".into()),
            ..Default::default()
        };
        let ok = is_stale_despite_liveness(
            AgentStatus::Live,
            Some(&progress),
            Some(1_000),
            1_000 + 999_999,
            3_600,
        );
        assert!(
            !ok,
            "a row with a real commit is never stale despite liveness"
        );
    }

    #[test]
    fn a_non_live_status_is_never_flagged() {
        let ok = is_stale_despite_liveness(
            AgentStatus::Exited,
            None,
            Some(1_000),
            1_000 + 999_999,
            3_600,
        );
        assert!(
            !ok,
            "a terminal row is not a liveness/progress disagreement"
        );
    }

    #[test]
    fn missing_clock_fails_toward_not_flagged() {
        let ok = is_stale_despite_liveness(AgentStatus::Live, None, None, 999_999, 3_600);
        assert!(
            !ok,
            "an unreliable clock must never manufacture a false alarm"
        );
    }
}
