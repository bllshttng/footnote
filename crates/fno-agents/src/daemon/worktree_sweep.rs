//! The daemon's worktree sweep: one report per repo on a 6h floor.
//! Report-only until a merge-minted cleanup order stands, then applying.
//! Moved out of daemon.rs (shrink-only file) with its tests; the event
//! carries `enumerated` and `judged` alongside the judged-bucket counts so a
//! truncated read cannot be told from a partial sweep.
use crate::events::EventEmitter;
use crate::paths::AgentsHome;
use serde_json::{json, Value};

/// How long between worktree report sweeps. A 24-hour reap order spans at
/// least three complete windows even when its mint cannot clear the stamp.
pub(crate) const WORKTREE_SWEEP_INTERVAL_SECS: u64 = 21_600;

/// Distinct canonical repo roots the registry knows about, deduplicated.
///
/// A linked worktree is not its own repo, so its rows fold into the checkout
/// that owns them and the sweep runs once per repo rather than once per row.
/// Each root is canonicalised before dedupe: raw registry paths could spell
/// the same repository four ways (symlinked bases, /var vs /private/var on
/// macOS), and one repo read as four roots swept four times per window and
/// collided on the sweep lock (measured 2026-09-14T22:31:45Z).
pub(crate) fn registry_repo_roots(home: &AgentsHome) -> Vec<String> {
    let Ok(loaded) = crate::state::load_registry(&home.registry_json()) else {
        return Vec::new();
    };
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for e in &loaded.entries {
        let raw = if e.project_root.is_empty() {
            e.cwd.clone()
        } else {
            e.project_root.clone()
        };
        if raw.is_empty() {
            continue;
        }
        let root = std::path::Path::new(&raw);
        if !root.is_dir() {
            continue;
        }
        let canonical = crate::paths::canonical_repo_root(root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or(raw);
        if std::path::Path::new(&canonical).is_dir() {
            seen.insert(canonical);
        }
    }
    // The request read spans the rotated generation too (merge_reap's reader),
    // so a repo whose only request rotated aside stays in the roots.
    for repo in crate::merge_reap::merge_cleanup_request_repos(home) {
        if std::path::Path::new(&repo).is_dir() {
            seen.insert(repo);
        }
    }
    seen.into_iter().collect()
}

/// One repo's worktree-sweep reading, parsed from the verb's `Summary:` line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorktreeSweepReport {
    pub eligible: usize,
    pub kept: usize,
    pub dirty: usize,
    pub enumerated: Option<usize>,
    pub judged: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSweepOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSweepOrderRead {
    pub standing: Option<bool>,
    pub exit_code: Option<i32>,
    pub stderr: String,
}

impl From<bool> for WorktreeSweepOrderRead {
    fn from(standing: bool) -> Self {
        Self {
            standing: Some(standing),
            exit_code: Some(0),
            stderr: String::new(),
        }
    }
}

/// Parse `fno agents workspace worktree cleanup --merged`'s summary line.
///
/// Returns `None` rather than a zeroed report when the line is absent. A sweep
/// that could not read its own output must not report "0 eligible, 0 dirty",
/// which is indistinguishable from a clean machine: an absence has two
/// explanations and a count must only ever come from a real reading.
///
/// The verb differs by mode (`would archive` dry-run vs `archived` apply), so
/// the eligible count reads from whichever the line carries. `enumerated` /
/// `judged` are Option: a summary from an older script carries neither, and
/// an absent token is never a fabricated zero.
pub fn parse_worktree_sweep(stdout: &str) -> Option<WorktreeSweepReport> {
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("Summary:"))?;
    let num_before = |needle: &str| -> Option<usize> {
        let idx = line.find(needle)?;
        line[..idx].split_whitespace().last()?.parse().ok()
    };
    let num_after = |needle: &str| -> Option<usize> {
        let idx = line.find(needle)?;
        line[idx + needle.len()..]
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    };
    let eligible = num_before(" would archive").or_else(|| num_before(" archived"))?;
    Some(WorktreeSweepReport {
        eligible,
        kept: num_before(" kept (")?,
        dirty: num_before(" dirty")?,
        enumerated: num_after("enumerated "),
        judged: num_after("judged "),
    })
}

/// Worktree sweep, one line per repo, on a 6h floor: report-only until a
/// merge-minted cleanup request stands, then applying.
///
/// A timer tick proves nothing on its own, so an unearned tick still only
/// REPORTS. Removal is merge-triggered: `fno do pr merge` (and the post-merge
/// ritual, as its second mint site) writes the `merge_cleanup_requested`
/// envelope, and while a pending request stands for a repository (`orders`
/// injects that scoped read) that repository's pass runs with `--apply`. The
/// primary consumer is the merge reaper (merge_reap.rs), which stops the
/// harness, drops the rows, and takes the tree; this sweep only catches what
/// that pass leaves behind. The sweep's own guards - reapable, live claim,
/// rooted processes - still decide tree by tree. There is no config knob,
/// because two off-switches for one decision strand whoever flips the wrong
/// one.
///
/// `orders` and `run` are injected so the policy is testable without shelling
/// out.
pub fn worktree_sweep(
    home: &AgentsHome,
    emitter: &EventEmitter,
    now: i64,
    roots: &[String],
    orders: &dyn Fn(&str) -> WorktreeSweepOrderRead,
    run: &dyn Fn(&str, bool) -> WorktreeSweepOutput,
) -> usize {
    let stamp = home.root().join("worktree-sweep.stamp");
    let last = std::fs::read_to_string(&stamp)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);
    if now.saturating_sub(last) < WORKTREE_SWEEP_INTERVAL_SECS as i64 {
        return 0;
    }
    let mut swept = 0;
    for root in roots {
        let order_read = orders(root);
        let Some(apply) = order_read.standing else {
            let stderr = order_read.stderr.lines().next().unwrap_or("");
            let _ = emitter.emit(
                "worktree_sweep",
                &json!({
                    "repo": root,
                    "error": "unreadable-orders",
                    "exit_code": order_read.exit_code,
                    "stderr": stderr,
                }),
            );
            continue;
        };
        let mode = if apply { "apply-orders" } else { "report-only" };
        // Emit for EVERY repo, including the ones that read zero. A tick that
        // stays silent when it finds nothing cannot be told from a tick that
        // never ran, and this sweep exists precisely to surface what the
        // ritual missed.
        let output = run(root, apply);
        let report = (output.exit_code == Some(0))
            .then(|| parse_worktree_sweep(&output.stdout))
            .flatten();
        match report {
            Some(r) => {
                let mut payload = json!({
                    "repo": root,
                    "eligible": r.eligible,
                    "kept": r.kept,
                    "dirty": r.dirty,
                    "mode": mode,
                });
                if let Some(n) = r.enumerated {
                    payload["enumerated"] = Value::from(n);
                }
                if let Some(n) = r.judged {
                    payload["judged"] = Value::from(n);
                }
                let _ = emitter.emit("worktree_sweep", &payload);
                swept += 1;
            }
            None => {
                let stderr = output.stderr.lines().next().unwrap_or("");
                let _ = emitter.emit(
                    "worktree_sweep",
                    &json!({
                        "repo": root,
                        "mode": mode,
                        "error": "unreadable-summary",
                        "exit_code": output.exit_code,
                        "stderr": stderr,
                    }),
                );
            }
        }
    }
    let _ = std::fs::write(&stamp, now.to_string());
    swept
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::AgentsHome;

    fn tmp_home(tag: &str) -> AgentsHome {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fno-agents-wt-sweep-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = AgentsHome::at(&p);
        home.ensure_root().unwrap();
        home
    }

    /// The real summary line, copied from this machine's output.
    const REAL_SUMMARY: &str = "would-archive      feature/sample-branch   /some/wt\n\
    Summary: 12 would archive, 37 kept (19 unmerged, 11 unpushed, 5 dirty, 0 live-session, 1 processes, 0 salvage-failed, 0 needs-confirmation, 1 app-owned, 1 permanent), 0 failed  [dry-run: no changes made; pass --apply to execute]\n";

    #[test]
    fn sweep_summary_parses_the_real_line() {
        let r = parse_worktree_sweep(REAL_SUMMARY).expect("parses");
        assert_eq!(r.eligible, 12);
        assert_eq!(r.kept, 37);
        assert_eq!(r.dirty, 5);
        assert_eq!(
            r.enumerated, None,
            "an old summary carries no enumerated token"
        );
        assert_eq!(r.judged, None);
    }

    #[test]
    fn sweep_summary_parses_the_apply_mode_line() {
        // The apply pass says "archived", not "would archive"; the eligible
        // count must read from whichever verb the line carries.
        let line = "archived         feature/sample-branch   /some/wt\n\
        Summary: 3 archived, 4 kept (1 unmerged, 1 unpushed, 1 dirty), 0 failed\n";
        let r = parse_worktree_sweep(line).expect("parses");
        assert_eq!(r.eligible, 3);
        assert_eq!(r.kept, 4);
        assert_eq!(r.dirty, 1);
    }

    #[test]
    fn sweep_summary_absent_is_none_not_zero() {
        // A zeroed report is indistinguishable from a clean machine. An absence
        // has two explanations and only a real reading may produce a count.
        assert!(parse_worktree_sweep("").is_none());
        assert!(parse_worktree_sweep("some other output\n").is_none());
    }

    #[test]
    fn sweep_summary_parses_enumerated_and_judged() {
        // AC13/AC14: the new tokens parse when present and stay None when the
        // line predates them.
        let line = "Summary: 1 would archive, 2 kept (1 unmerged, 1 dirty), 0 failed, enumerated 3 judged 3  [dry-run: no changes made; pass --apply to execute]\n";
        let r = parse_worktree_sweep(line).expect("parses");
        assert_eq!(r.eligible, 1);
        assert_eq!(r.kept, 2);
        assert_eq!(r.dirty, 1);
        assert_eq!(r.enumerated, Some(3));
        assert_eq!(r.judged, Some(3));
    }

    #[test]
    fn sweep_reports_every_repo_including_the_quiet_ones() {
        let home = tmp_home("quiet");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let quiet =
            "Summary: 0 would archive, 0 kept (0 unmerged, 0 unpushed, 0 dirty), 0 failed\n";

        let swept = worktree_sweep(
            &home,
            &emitter,
            1_000_000,
            &["/repo/a".into(), "/repo/b".into()],
            &|_| false.into(),
            &|_, _| WorktreeSweepOutput {
                exit_code: Some(0),
                stdout: quiet.into(),
                stderr: String::new(),
            },
        );

        assert_eq!(swept, 2, "a tick that finds nothing must still report");
        let log = crate::events::committed_journal_text(&home.events_jsonl());
        assert_eq!(log.matches("worktree_sweep").count(), 2);
        assert!(log.contains("report-only"));
        assert!(!log.contains("apply-orders"));
    }

    #[test]
    fn sweep_applies_only_when_a_reap_order_stands() {
        // Ruling preserved: a merged PR is proof, a timer tick is not. The
        // timer lane applies ONLY when the merge ritual minted an order.
        let home = tmp_home("ordered");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let quiet =
            "Summary: 0 would archive, 0 kept (0 unmerged, 0 unpushed, 0 dirty), 0 failed\n";

        let swept = worktree_sweep(
            &home,
            &emitter,
            1_000_000,
            &["/repo/a".into()],
            &|_| true.into(),
            &|_, apply| {
                assert!(apply, "a standing order must reach the verb as --apply");
                WorktreeSweepOutput {
                    exit_code: Some(0),
                    stdout: quiet.into(),
                    stderr: String::new(),
                }
            },
        );

        assert_eq!(swept, 1);
        let log = crate::events::committed_journal_text(&home.events_jsonl());
        assert!(log.contains("apply-orders"));
        assert!(!log.contains("report-only"));
    }

    #[test]
    fn sweep_reads_reap_orders_in_each_repository_scope() {
        let home = tmp_home("repo-orders");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let seen = std::sync::Mutex::new(Vec::new());
        let quiet =
            "Summary: 0 would archive, 0 kept (0 unmerged, 0 unpushed, 0 dirty), 0 failed\n";

        let swept = worktree_sweep(
            &home,
            &emitter,
            1_000_000,
            &["/repo/a".into(), "/repo/b".into()],
            &|root| (root == "/repo/b").into(),
            &|root, apply| {
                seen.lock().unwrap().push((root.to_string(), apply));
                WorktreeSweepOutput {
                    exit_code: Some(0),
                    stdout: quiet.into(),
                    stderr: String::new(),
                }
            },
        );

        assert_eq!(swept, 2);
        assert_eq!(
            seen.into_inner().unwrap(),
            vec![("/repo/a".into(), false), ("/repo/b".into(), true)]
        );
    }

    #[test]
    fn sweep_skips_a_repo_when_its_order_probe_is_unreadable() {
        let home = tmp_home("order-unreadable");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let ran_cleanup = std::sync::atomic::AtomicBool::new(false);

        let swept = worktree_sweep(
            &home,
            &emitter,
            1_000_000,
            &["/repo/a".into()],
            &|_| WorktreeSweepOrderRead {
                standing: None,
                exit_code: Some(7),
                stderr: "claim store unreadable\nextra detail\n".into(),
            },
            &|_, _| {
                ran_cleanup.store(true, std::sync::atomic::Ordering::Relaxed);
                unreachable!("an unreadable order probe must skip cleanup")
            },
        );

        assert_eq!(swept, 0);
        assert!(!ran_cleanup.load(std::sync::atomic::Ordering::Relaxed));
        let log = crate::events::committed_journal_text(&home.events_jsonl());
        assert!(log.contains("\"error\":\"unreadable-orders\""));
        assert!(log.contains("\"exit_code\":7"));
        assert!(log.contains("\"stderr\":\"claim store unreadable\""));
        assert!(!log.contains("extra detail"));
        assert!(!log.contains("report-only"));
    }

    #[test]
    fn sweep_honours_its_own_6h_floor() {
        let home = tmp_home("floor");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let out = |_: &str, _: bool| WorktreeSweepOutput {
            exit_code: Some(0),
            stdout: REAL_SUMMARY.into(),
            stderr: String::new(),
        };
        let now = 1_000_000;

        assert_eq!(
            worktree_sweep(
                &home,
                &emitter,
                now,
                &["/repo/a".into()],
                &|_| false.into(),
                &out,
            ),
            1
        );
        // Same window: skipped entirely, no second reading.
        assert_eq!(
            worktree_sweep(
                &home,
                &emitter,
                now + 60,
                &["/repo/a".into()],
                &|_| false.into(),
                &out
            ),
            0
        );
        // A little over six hours later: fires again.
        assert_eq!(
            worktree_sweep(
                &home,
                &emitter,
                now + 21_601,
                &["/repo/a".into()],
                &|_| false.into(),
                &out
            ),
            1
        );
    }

    #[test]
    fn sweep_never_passes_apply_on_its_own_authority() {
        // Ruling: a merged PR is proof, a timer tick is not. The fn body may
        // not carry an --apply literal: applying is decided by the injected
        // orders read (merge-minted claims), never by the sweep itself.
        let src = include_str!("worktree_sweep.rs");
        let idx = src
            .find("pub fn worktree_sweep(")
            .expect("worktree_sweep exists");
        let body = &src[idx..idx + 2000.min(src.len() - idx)];
        assert!(!body.contains("--apply"));
    }

    #[test]
    fn sweep_records_an_unreadable_summary_rather_than_inventing_zeros() {
        let home = tmp_home("unreadable");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");

        let swept = worktree_sweep(
            &home,
            &emitter,
            1_000_000,
            &["/repo/a".into()],
            &|_| false.into(),
            &|_, _| WorktreeSweepOutput {
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
            },
        );

        assert_eq!(swept, 0);
        let log = crate::events::committed_journal_text(&home.events_jsonl());
        assert!(log.contains("unreadable-summary"));
        assert!(!log.contains("\"eligible\""));
    }

    #[test]
    fn sweep_records_a_nonzero_exit_and_first_stderr_line() {
        let home = tmp_home("nonzero");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let output = WorktreeSweepOutput {
            exit_code: Some(7),
            stdout: String::new(),
            stderr: "permission denied\nextra detail\n".into(),
        };

        let swept = worktree_sweep(
            &home,
            &emitter,
            1_000_000,
            &["/repo/a".into()],
            &|_| false.into(),
            &|_, _| output.clone(),
        );

        assert_eq!(swept, 0);
        let log = crate::events::committed_journal_text(&home.events_jsonl());
        assert!(log.contains("\"exit_code\":7"));
        assert!(log.contains("\"stderr\":\"permission denied\""));
        assert!(!log.contains("extra detail"));
    }

    #[test]
    fn sweep_distinguishes_a_zero_exit_with_no_summary() {
        let home = tmp_home("zero-no-summary");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");

        let swept = worktree_sweep(
            &home,
            &emitter,
            1_000_000,
            &["/repo/a".into()],
            &|_| false.into(),
            &|_, _| WorktreeSweepOutput {
                exit_code: Some(0),
                stdout: "no summary here\n".into(),
                stderr: String::new(),
            },
        );

        assert_eq!(swept, 0);
        let log = crate::events::committed_journal_text(&home.events_jsonl());
        assert!(log.contains("\"exit_code\":0"));
        assert!(log.contains("\"stderr\":\"\""));
    }
}
