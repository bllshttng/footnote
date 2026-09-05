//! Row retirement, keyed by the reverse join through `node.sessions[]`
//! (x-c672).
//!
//! A worker's registry row leaves when its WORK is done and its transcript
//! is quiet. WORK-done is the graph's `status == "done"` read through the
//! reverse join ([`crate::graph_store::work_state`]) over every node the
//! session is named on; quiet is the served transcript mtime past the retire
//! grace. Nothing waits for a session to end, because a session never ends
//! (d-10a72d88): the exit-stamp machinery this module used to carry
//! (`exited_at`, `StampExit`, corroboration gates, the backstop, the dormant
//! probe) asked a question with no answer and is deleted.
//!
//! The row question and the worktree question are DIFFERENT questions with
//! different keys. A row retires on work-done plus quiet regardless of its
//! worktree; the tree is then governed by its own bucket (dirty never,
//! clean-and-unmerged never, clean-and-merged loses the tree and keeps the
//! branch), so a dirty tree never pins a finished row and a row retirement
//! never destroys an unmerged branch's only checkout.
//!
//! This module holds the pure decision plus the sweep shells. The sweep body
//! lives in `gc_sweep.rs`. All I/O - the graph read, the transcript stat, the
//! stop, the worktree probes, the clock - is injected, so the policy is
//! unit-testable in isolation.

use crate::graph_store::WorkState;

/// The row verdict: retire now, or keep with a named reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcAction {
    /// The row leaves the registry: work done on every named node, transcript
    /// quiet past the grace. The receipt records the resumable handle first.
    Retire,
    /// The row stays, and [`KeepReason`] names the gate holding it.
    Keep,
}

/// The probed facts about one registry row the retirement policy needs.
#[derive(Debug, Clone)]
pub struct GcRow {
    /// The row's `origin` (state.rs): only `"operator"` protects, and only
    /// `"spawn"` retires; `None` is not the same fact as either.
    pub origin: Option<String>,
    /// An orchestrator crown rides this row (US9). A crowned row is never
    /// retired by a sweep.
    pub crowned: bool,
    /// WORK-done through the reverse join: named on which nodes, and are they
    /// all `done`.
    pub work: WorkState,
    /// Seconds since the row's transcript was last written, from the served
    /// harness store. `None` = unresolved, and an unresolved transcript is
    /// NEVER quiet.
    pub transcript_age_s: Option<i64>,
    /// Does this row own a REMOVABLE worktree? False for a one-shot ask, a
    /// row in the canonical checkout, or a cwd that is not a linked worktree.
    pub owns_worktree: bool,
    /// Worktree cleanliness: `Some(true)` clean, `Some(false)` dirty,
    /// `None` the probe could not answer (fail closed -> tree kept).
    pub worktree_clean: Option<bool>,
    /// Is the worktree's branch merged into the main line? `Some(true)`,
    /// `Some(false)`, `None` (nothing names the work or the main line).
    /// Asked only after cleanliness answered `true`.
    pub branch_merged: Option<bool>,
}

/// WHICH gate is holding a [`GcAction::Keep`] row. Every keep is named - a
/// row that is stuck and invisible is the failure mode this enum exists to
/// prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeepReason {
    /// `origin: operator`: a human's row, never touched by a sweep.
    Operator,
    /// A crowned orchestrator row.
    Crowned,
    /// Origin is not `spawn` (adopted, or nothing recorded): only a row fno
    /// itself spawned retires, whatever the work state says.
    NotSpawn { origin: String },
    /// The session is named in no node's `sessions[]`: no provenance, so no
    /// work-done verdict is possible (d-5de7067a's refuse-without-provenance).
    NoProvenance,
    /// At least one named node is not done; the first open one is reported.
    OpenWork { node: String, status: String },
    /// The transcript was written inside the grace window: the session is
    /// live in the only sense the law allows.
    Active { age_s: i64 },
    /// The transcript could not be resolved. Absence is not quiet.
    TranscriptUnresolved,
    /// The graph could not be read this sweep. Never a retirement on a
    /// failed read.
    GraphUnreadable,
    /// Every named node is done but one still carries an OPEN do row for
    /// this session: settled work would be re-opened by the retirement's
    /// absence, so the row stays and the node is named.
    OpenDoRow { node: String },
}

impl KeepReason {
    /// Stable, human-readable tag for CLI/JSON output.
    pub fn as_str(&self) -> &'static str {
        match self {
            KeepReason::Operator => "operator",
            KeepReason::Crowned => "crowned",
            KeepReason::NotSpawn { .. } => "not a spawn row",
            KeepReason::NoProvenance => "no provenance: named in no node",
            KeepReason::OpenWork { .. } => "open work",
            KeepReason::Active { .. } => "active",
            KeepReason::TranscriptUnresolved => "transcript unresolved",
            KeepReason::GraphUnreadable => "graph unreadable",
            KeepReason::OpenDoRow { .. } => "open do row on done node",
        }
    }
}

/// The tree verdict for a RETIRED row's worktree. Asked only after the row
/// verdict is `Retire`; a keep-shaped tree never blocks the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeAction {
    /// Remove the tree, keep the branch: clean and merged.
    Prune,
    /// Dirty: uncommitted or untracked content. Tree kept, named.
    KeepDirty,
    /// Clean but the branch never merged: abandoned-but-real work a human
    /// judges. Tree kept, named.
    KeepUnmerged,
    /// The cleanliness probe could not answer. Tree kept, named.
    KeepUnprobed,
    /// The row owns no removable worktree.
    None,
}

/// The one row decision. Order matters and each gate names itself: operator,
/// crown, provenance, open work, transcript, grace, retire. No boolean folds
/// two questions together.
pub fn gc_decide(row: &GcRow, grace_secs: i64) -> (GcAction, Option<KeepReason>) {
    if row.origin.as_deref() == Some("operator") {
        return (GcAction::Keep, Some(KeepReason::Operator));
    }
    if row.crowned {
        return (GcAction::Keep, Some(KeepReason::Crowned));
    }
    // Only a row fno itself spawned retires. An `adopted` row (a session the
    // operator took over) or a row with no origin recorded is someone else's
    // fact about a session, and done-plus-quiet does not make it fno's to
    // remove.
    if row.origin.as_deref() != Some("spawn") {
        let origin = row.origin.clone().unwrap_or_default();
        return (GcAction::Keep, Some(KeepReason::NotSpawn { origin }));
    }
    match &row.work {
        WorkState::NoProvenance => (GcAction::Keep, Some(KeepReason::NoProvenance)),
        WorkState::Open { node, status } => (
            GcAction::Keep,
            Some(KeepReason::OpenWork {
                node: node.clone(),
                status: status.clone(),
            }),
        ),
        WorkState::AllDone { .. } => match row.transcript_age_s {
            None => (GcAction::Keep, Some(KeepReason::TranscriptUnresolved)),
            Some(age) if age <= grace_secs => {
                (GcAction::Keep, Some(KeepReason::Active { age_s: age }))
            }
            Some(_) => (GcAction::Retire, None),
        },
    }
}

/// The tree verdict for a row the policy just retired. Runs ONLY on Retire:
/// row retirement makes a tree eligible and never bypasses the bucket.
pub fn tree_action(row: &GcRow) -> TreeAction {
    if !row.owns_worktree {
        return TreeAction::None;
    }
    match row.worktree_clean {
        None => TreeAction::KeepUnprobed,
        Some(false) => TreeAction::KeepDirty,
        Some(true) => match row.branch_merged {
            Some(true) => TreeAction::Prune,
            _ => TreeAction::KeepUnmerged,
        },
    }
}

/// The one handle a row is both PROBED and REPORTED under. `fno agents truth`
/// resolves a row by short_id or by name, so the fallback is a real handle,
/// not a display string. Written once so the sweep never probes under one
/// name and reports under another.
pub(crate) fn row_handle(e: &crate::state::RegistryEntry) -> String {
    if e.short_id.is_empty() {
        e.name.clone()
    } else {
        e.short_id.clone()
    }
}

/// Seconds since the newest of the store's matches was written. Newest, not
/// first: a session can leave stubs in other project dirs, and a stub whose
/// creation post-dates the real transcript's last turn must not read as
/// fresher than it is. `None` when no match resolves: an unresolved
/// transcript is never a quiet one.
pub(crate) fn transcript_age_s(store_hits: Option<&[std::path::PathBuf]>, now: i64) -> Option<i64> {
    let newest = store_hits?.iter().max_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0)
    })?;
    let mtime = std::fs::metadata(newest)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)?;
    Some(now.saturating_sub(mtime))
}

// --- the sweep shells -------------------------------------------------------
// The two triggers behind the pure decision above: the daemon's idle tick and
// the manual `fno agents reap` verb both shell these. The sweep body lives in
// `gc_sweep.rs`; these resolve the production seams (graph read, store index,
// stop lane) and call it.

use crate::events::EventEmitter;
use crate::gc_sweep;
use crate::paths::AgentsHome;

/// The daemon idle tick's retirement sweep: classify every row, retire the
/// work-done-and-quiet ones (stop the held process first), prune their
/// clean-and-merged worktrees, and write the receipt every removal needs to
/// stay reversible.
pub fn gc_sweep(
    home: &AgentsHome,
    emitter: &EventEmitter,
    grace_secs: i64,
    retain_days: u64,
) -> gc_sweep::GcSummary {
    let store = std::cell::RefCell::new(gc_sweep::HarnessStoreIndex::default());
    gc_sweep::run(
        home,
        emitter,
        grace_secs,
        false,
        retain_days,
        &gc_sweep::read_graph_entries,
        &|e| store.borrow_mut().matches(e),
        &|e| gc_sweep::stop_row_process(home, e),
        &gc_sweep::production_tree_probe,
        &|e| {
            crate::daemon::rm_take_worktree(e);
        },
    )
}

/// `fno agents reap --dry-run`: classify exactly as [`gc_sweep`] does, name
/// every row under exactly one bucket, mutate nothing - a reaper an operator
/// cannot rehearse is one they will not run.
pub fn gc_sweep_dry_run(home: &AgentsHome, grace_secs: i64) -> gc_sweep::GcSummary {
    // Never emitted to in dry-run mode (the whole write+emit tail is skipped),
    // so an unused placeholder path satisfies the shared signature.
    let emitter = EventEmitter::new(std::path::PathBuf::new(), "daemon");
    let store = std::cell::RefCell::new(gc_sweep::HarnessStoreIndex::default());
    gc_sweep::run(
        home,
        &emitter,
        grace_secs,
        true,
        0, // dry-run never expires: a rehearsal that pruned would not be one
        &gc_sweep::read_graph_entries,
        &|e| store.borrow_mut().matches(e),
        &|e| gc_sweep::stop_row_process(home, e),
        &gc_sweep::production_tree_probe,
        &|e| {
            crate::daemon::rm_take_worktree(e);
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRACE: i64 = 900;

    /// A spawn-origin, uncrowned row whose work is done everywhere it is
    /// named and whose transcript has been quiet past the grace: the AC3-HP
    /// base case.
    fn retiring() -> GcRow {
        GcRow {
            origin: Some("spawn".into()),
            crowned: false,
            work: WorkState::AllDone {
                nodes: vec!["N1".into()],
            },
            transcript_age_s: Some(GRACE + 1),
            owns_worktree: true,
            worktree_clean: Some(true),
            branch_merged: Some(true),
        }
    }

    #[test]
    fn ac3_hp_work_done_and_quiet_retires_and_prunes() {
        let row = retiring();
        assert_eq!(gc_decide(&row, GRACE), (GcAction::Retire, None));
        assert_eq!(tree_action(&row), TreeAction::Prune);
    }

    #[test]
    fn stop_row_process_inside_an_ambient_block_on_does_not_panic() {
        // The CLI verb runs the sweep under main's block_on, so the stop seam
        // executes on a thread that already drives a runtime. The re-entrant
        // block_on that shape used to hit panicked the reap verb before it
        // classified a single row; the dedicated thread keeps it silent. A
        // default entry owns no socket, so nothing answers and nothing is
        // stopped - the subject is the absence of a panic, not the verdict.
        let dir = std::env::temp_dir().join(format!(
            "fno-gc-stop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = AgentsHome::at(&dir);
        home.ensure_root().unwrap();
        let entry = crate::state::RegistryEntry::default();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _stopped = rt.block_on(async { gc_sweep::stop_row_process(&home, &entry) });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dry_run_never_stops_a_retiring_row() {
        // A rehearsal that killed the worker it rehearsed retiring would be
        // the destructive run wearing a dry flag. The stop seam must never
        // fire in dry-run, even for a row that classifies Retire end to end.
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let dir = std::env::temp_dir().join(format!(
            "fno-gc-dry-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = AgentsHome::at(&dir);
        home.ensure_root().unwrap();
        // A transcript quiet past grace: written, then aged 2000s.
        let transcript = dir.join("rollout.jsonl");
        std::fs::write(&transcript, b"{}\n").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2000);
        std::fs::File::options()
            .write(true)
            .open(&transcript)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        crate::state::update_registry(&home.registry_json(), |r| {
            let mut e = crate::state::RegistryEntry::default();
            e.name = "dryw".into();
            e.short_id = "dryw".into();
            e.origin = Some("spawn".into());
            e.harness = Some("codex".into());
            e.harness_session_id = Some("S-dry".into());
            r.entries.push(e);
        })
        .unwrap();
        let mut index = HashMap::new();
        index.insert(
            "s-dry".to_string(),
            vec![("N1".to_string(), "done".to_string())],
        );
        let graph = std::cell::RefCell::new(Some(gc_sweep::GraphRead {
            index,
            open_do: HashMap::new(),
        }));
        let emitter = crate::events::EventEmitter::new(std::path::PathBuf::new(), "daemon");
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stopped);
        let summary = gc_sweep::run(
            &home,
            &emitter,
            900,
            true,
            7,
            &|_h| graph.borrow_mut().take(),
            &|_e| Some(vec![transcript.clone()]),
            &move |_e| {
                flag.store(true, Ordering::SeqCst);
                true
            },
            &|_e| (None, None),
            &|_e| {},
        );
        assert!(
            !stopped.load(Ordering::SeqCst),
            "the stop seam fired in dry-run"
        );
        assert_eq!(
            summary.retired.len(),
            1,
            "the row still classifies would-retire"
        );
        assert!(
            crate::state::load_registry(&home.registry_json())
                .unwrap()
                .entries
                .iter()
                .any(|e| e.name == "dryw"),
            "dry-run mutated the registry"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_non_spawn_row_names_its_own_gate_even_when_the_graph_is_unreadable() {
        // The origin gate runs BEFORE the graph read in the sweep, so a row
        // fno never spawned is held under not-a-spawn-row whatever the
        // graph's state - kept_graph_unreadable must not shadow the policy's
        // own first gate (gc_decide checks origin before work).
        let dir = std::env::temp_dir().join(format!(
            "fno-gc-notspawn-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = AgentsHome::at(&dir);
        home.ensure_root().unwrap();
        crate::state::update_registry(&home.registry_json(), |r| {
            let mut e = crate::state::RegistryEntry::default();
            e.name = "adop".into();
            e.short_id = "adop".into();
            e.origin = Some("adopted".into());
            e.harness = Some("codex".into());
            e.harness_session_id = Some("S-adop".into());
            r.entries.push(e);
        })
        .unwrap();
        let emitter = crate::events::EventEmitter::new(std::path::PathBuf::new(), "daemon");
        let summary = gc_sweep::run(
            &home,
            &emitter,
            900,
            false,
            7,
            // The graph read answers NOTHING: every row would class as
            // graph-unreadable if the origin gate did not run first.
            &|_| None,
            &|_| None,
            &|_| true,
            &|_| (None, None),
            &|_| {},
        );
        assert_eq!(
            summary.kept_not_spawn,
            vec![("adop".to_string(), "adopted".to_string())]
        );
        assert!(
            summary.kept_graph_unreadable.is_empty(),
            "{:?}",
            summary.kept_graph_unreadable
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ac3_edge_operator_crown_open_work_and_active_keep() {
        let operator = GcRow {
            origin: Some("operator".into()),
            ..retiring()
        };
        assert_eq!(
            gc_decide(&operator, GRACE),
            (GcAction::Keep, Some(KeepReason::Operator))
        );

        let crowned = GcRow {
            crowned: true,
            ..retiring()
        };
        assert_eq!(
            gc_decide(&crowned, GRACE),
            (GcAction::Keep, Some(KeepReason::Crowned))
        );

        // Only a spawn row retires: adopted (and an unrecorded origin) keep
        // whatever the work state says - done-plus-quiet is not fno's to act
        // on for a session it did not spawn.
        for origin in ["adopted", "operator", ""] {
            let not_spawn = GcRow {
                origin: if origin.is_empty() {
                    None
                } else {
                    Some(origin.into())
                },
                ..retiring()
            };
            let (action, reason) = gc_decide(&not_spawn, GRACE);
            assert_eq!(action, GcAction::Keep, "origin {origin:?}");
            assert!(
                matches!(
                    reason,
                    Some(KeepReason::NotSpawn { .. }) | Some(KeepReason::Operator)
                ),
                "origin {origin:?} named its gate"
            );
        }

        let open = GcRow {
            work: WorkState::Open {
                node: "N3".into(),
                status: "in_review".into(),
            },
            ..retiring()
        };
        assert_eq!(
            gc_decide(&open, GRACE),
            (
                GcAction::Keep,
                Some(KeepReason::OpenWork {
                    node: "N3".into(),
                    status: "in_review".into()
                })
            )
        );

        // A transcript written 10 seconds ago: the session is writing, keep.
        let active = GcRow {
            transcript_age_s: Some(10),
            ..retiring()
        };
        assert_eq!(
            gc_decide(&active, GRACE),
            (GcAction::Keep, Some(KeepReason::Active { age_s: 10 }))
        );
        // Exactly at the grace boundary is still inside it.
        let boundary = GcRow {
            transcript_age_s: Some(GRACE),
            ..retiring()
        };
        assert_eq!(
            gc_decide(&boundary, GRACE),
            (GcAction::Keep, Some(KeepReason::Active { age_s: GRACE }))
        );
    }

    #[test]
    fn ac3_err_unresolved_transcript_no_provenance_and_tree_buckets() {
        let unresolved = GcRow {
            transcript_age_s: None,
            ..retiring()
        };
        assert_eq!(
            gc_decide(&unresolved, GRACE),
            (GcAction::Keep, Some(KeepReason::TranscriptUnresolved))
        );

        let no_provenance = GcRow {
            work: WorkState::NoProvenance,
            ..retiring()
        };
        assert_eq!(
            gc_decide(&no_provenance, GRACE),
            (GcAction::Keep, Some(KeepReason::NoProvenance))
        );

        // The tree buckets, on rows that DID retire: a dirty or unmerged or
        // unprobed tree never blocks the row, it only keeps the tree.
        let dirty = GcRow {
            worktree_clean: Some(false),
            ..retiring()
        };
        assert_eq!(gc_decide(&dirty, GRACE), (GcAction::Retire, None));
        assert_eq!(tree_action(&dirty), TreeAction::KeepDirty);

        let unmerged = GcRow {
            branch_merged: Some(false),
            ..retiring()
        };
        assert_eq!(gc_decide(&unmerged, GRACE), (GcAction::Retire, None));
        assert_eq!(tree_action(&unmerged), TreeAction::KeepUnmerged);

        let unprobed = GcRow {
            worktree_clean: None,
            ..retiring()
        };
        assert_eq!(gc_decide(&unprobed, GRACE), (GcAction::Retire, None));
        assert_eq!(tree_action(&unprobed), TreeAction::KeepUnprobed);

        let detached = GcRow {
            branch_merged: None,
            ..retiring()
        };
        assert_eq!(tree_action(&detached), TreeAction::KeepUnmerged);

        // A row owning nothing removable: no tree question at all.
        let bare = GcRow {
            owns_worktree: false,
            ..retiring()
        };
        assert_eq!(tree_action(&bare), TreeAction::None);

        // origin None is NOT the spawn fact: with no origin recorded, done
        // work and a quiet transcript still keep the row - only an explicit
        // spawn origin retires, and retiring() above carries one.
        let unstamped = GcRow {
            origin: None,
            ..retiring()
        };
        assert_eq!(
            gc_decide(&unstamped, GRACE),
            (
                GcAction::Keep,
                Some(KeepReason::NotSpawn { origin: "".into() })
            )
        );
    }

    #[test]
    fn keep_reason_tags_name_their_gate() {
        assert_eq!(KeepReason::Operator.as_str(), "operator");
        assert_eq!(KeepReason::Crowned.as_str(), "crowned");
        assert_eq!(
            KeepReason::NoProvenance.as_str(),
            "no provenance: named in no node"
        );
        assert_eq!(
            KeepReason::OpenWork {
                node: "N".into(),
                status: "ready".into()
            }
            .as_str(),
            "open work"
        );
        assert_eq!(KeepReason::Active { age_s: 5 }.as_str(), "active");
        assert_eq!(
            KeepReason::TranscriptUnresolved.as_str(),
            "transcript unresolved"
        );
        assert_eq!(KeepReason::GraphUnreadable.as_str(), "graph unreadable");
        assert_eq!(
            KeepReason::OpenDoRow { node: "N".into() }.as_str(),
            "open do row on done node"
        );
    }

    #[test]
    fn transcript_age_reads_the_newest_store_match() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.jsonl");
        let fresh = dir.path().join("fresh.jsonl");
        std::fs::write(&old, "{}").unwrap();
        std::fs::write(&fresh, "{}").unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let hits = vec![old, fresh];
        let age = transcript_age_s(Some(&hits), now).unwrap();
        assert!(
            age < 5,
            "age {age} should be ~0 for a just-written transcript"
        );
        assert_eq!(transcript_age_s(None, now), None);
        assert_eq!(transcript_age_s(Some(&[]), now), None);
    }
}
