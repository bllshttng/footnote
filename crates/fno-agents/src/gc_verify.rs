//! The retirement verifier (x-70e1 task 5): a READ-ONLY audit of the
//! receipts store that answers "did the currently deployed build actually
//! retire, resumably, in this window" - and refuses everything else.
//!
//! A pass requires, for every receipt in the window: a build stamp naming
//! the CURRENT binary (a dry run, an older daemon, a fixture run never
//! counts), the FULL required op set (`native-stop`, `active-surface`,
//! `resume-evidence`, each at a confirmed or measured not-applicable
//! outcome), and nothing unconfirmed. An empty store, an unreadable
//! receipt, a partial effect set: each is a named refusal with a nonzero
//! exit - this verb must fail before its evidence exists, because it is
//! the plan's done probe. The required set is what closes the x-5aef hole:
//! the auditor's synthetic receipt carried one op and the gate certified
//! it; a pass now means the promised outcome, not a nonempty list.
//!
//! A stale build (a receipt stamped by any other binary, including the
//! previous deploy) SKIPS: it can never verify, and it must not rebrand the
//! fleet's rollout tail as failure - during any rollout the window holds
//! both builds' receipts, and those refusals would hold the probe red for
//! a full window. Only current-build receipts enter `verified`, so a
//! window of nothing but stale receipts still fails on empty evidence.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::paths::AgentsHome;
use crate::receipt::read_reap_receipt;

/// One receipt's audit line.
#[derive(Debug)]
pub struct VerifiedRetirement {
    pub harness: String,
    pub session_id: String,
    pub row_name: String,
    pub reaped_at: String,
    pub writer_build: String,
    pub effects: Vec<String>,
}

/// Why one receipt in the window does not verify.
#[derive(Debug)]
pub struct VerifyProblem {
    pub receipt: String,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct VerifyReport {
    pub checked: usize,
    pub verified: Vec<VerifiedRetirement>,
    pub problems: Vec<VerifyProblem>,
    /// Receipts not this verifier's population (stale build): named, never
    /// verified, and never a refusal.
    pub skipped: Vec<VerifyProblem>,
    /// The expected cohort (`--expect-sessions`), as given (AC2). Empty
    /// contributes nothing.
    pub expected: Vec<String>,
    /// Expected sessions with no verified retirement (AC2-HP).
    pub missing: Vec<String>,
}

impl VerifyReport {
    /// A pass needs at least one verified retirement and zero problems: an
    /// empty window is not a clean bill, it is no evidence at all.
    pub fn passes(&self) -> bool {
        !self.verified.is_empty() && self.problems.is_empty()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "passes": self.passes(),
            "checked": self.checked,
            "verified": self.verified.iter().map(|v| json!({
                "harness": v.harness,
                "session_id": v.session_id,
                "row_name": v.row_name,
                "reaped_at": v.reaped_at,
                "writer_build": v.writer_build,
                "effects": v.effects,
            })).collect::<Vec<_>>(),
            "problems": self.problems.iter().map(|p| json!({
                "receipt": p.receipt,
                "reason": p.reason,
            })).collect::<Vec<_>>(),
            "skipped": self.skipped.iter().map(|p| json!({
                "receipt": p.receipt,
                "reason": p.reason,
            })).collect::<Vec<_>>(),
            "expected": self.expected,
            "missing": self.missing,
            "build": current_build(),
        })
    }
}

/// The identity of the BUILD, not of the running executable: the pin every
/// receipt in the window must carry.
///
/// The writer and the verifier are never the same file. `fno-agents-daemon`
/// writes the receipt; `fno-agents` runs `reap --verify`. One `cargo install`
/// lays down three separate executables, so any pin read off the running
/// exe (its path, its mtime, its own hash) differs between writer and reader
/// and the audit can never pass. It did not: a 523-receipt window read 0
/// verified, every current-daemon receipt named stale over a 5-second mtime
/// gap between the two binaries of one install.
///
/// So the pin is what the whole triad bakes at compile time: the crates/
/// subtree rev from build.rs, the same quantity `fno doctor update` already
/// uses to prove the three bins are ONE build. Two deployments of the same
/// package version still differ whenever the source moved. A rebuild of
/// identical source shares a stamp, which is correct - it is the same build.
/// The `-dirty` suffix names an uncommitted tree so a dev build never passes
/// itself off as the committed rev.
///
/// The known limit: a build with no git checkout to read (a crates.io
/// tarball) bakes the rev `unknown`, so every such build of one package
/// version shares a pin. The audit then verifies at version granularity
/// there. That is the weaker end of the trade, and it is the end that still
/// works: pinning harder than the triad can agree on is what made the audit
/// unpassable in the first place.
pub fn current_build() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let rev = env!("FNO_AGENTS_CRATES_REV");
    let dirty = if env!("FNO_AGENTS_GIT_DIRTY") == "1" {
        "-dirty"
    } else {
        ""
    };
    format!("fno-agents {version} rev {rev}{dirty}")
}

/// The ops every verified retirement must carry (x-5aef task 2.1): a pass
/// means the promised outcome, not a nonempty list. The auditor's synthetic
/// receipt carried `active-surface` alone and the old nonempty check
/// certified it. `resume-evidence` is what makes the receipt a recovery
/// record instead of an obituary: absent or failed, the gate refuses.
///
/// Mux is named in the refusal text as context, never as a requirement: no
/// fno-agents call site emits a mux effect yet (owner: x-7649).
pub const REQUIRED_OPS: [&str; 3] = ["native-stop", "active-surface", "resume-evidence"];

/// The outcomes that count as applied (or measured not-applicable) for a
/// required op.
fn outcome_confirmed(outcome: &str) -> bool {
    matches!(
        outcome,
        "confirmed-removed" | "confirmed-already-absent" | "not-applicable"
    )
}

/// Audit the receipts store over the window. Read-only: nothing here writes.
/// `expected` names the cohort: when nonempty, every named session must
/// appear among the verified retirements (AC2) - absent, the check
/// contributes nothing.
pub fn verify(home: &AgentsHome, since_secs: u64, expected: &[String]) -> VerifyReport {
    let mut report = VerifyReport::default();
    report.expected = expected.to_vec();
    let dir = home.root().join("reap-receipts");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        report.problems.push(VerifyProblem {
            receipt: dir.to_string_lossy().to_string(),
            reason: "receipts store absent: no retirement has ever been applied".into(),
        });
        return report;
    };
    let now = chrono::Utc::now();
    let build = current_build();
    let mut skipped_builds: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    paths.sort();
    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        audit_receipt(
            &mut report,
            &path,
            &name,
            now,
            since_secs,
            build.as_str(),
            &mut skipped_builds,
        );
    }
    // A window that verified nothing and refused nothing reads as a silent
    // red: `problems` empty, `passes` false, and no reason on the page. Name
    // the shape, with the counts that separate its causes. On the live fleet
    // this exact output (523 checked, 0 verified, 0 problems) hid a build pin
    // that could never match, and reading it took a source dive.
    if report.verified.is_empty() && report.problems.is_empty() {
        // Every in-window receipt read here was written by another build:
        // name it and the remedy, since this string is the only thing an
        // operator reads when the window is red.
        let remedy = if skipped_builds.is_empty() {
            String::new()
        } else {
            format!(
                ". Written by: {}. Run `fno doctor update` to deploy the current build ({build:?}).",
                skipped_builds.into_iter().collect::<Vec<_>>().join(", ")
            )
        };
        report.problems.push(VerifyProblem {
            receipt: format!("window of {since_secs}s"),
            reason: format!(
                "no retirement by the current build to verify: {} receipt(s) read, \
                 {} from another build, none stamped {build:?}{remedy}",
                report.checked,
                report.skipped.len()
            ),
        });
    }
    audit_cohort(&mut report);
    report
}

/// Audit one receipt: window, population, build stamp, and the required op
/// set. Each unmet condition is a named refusal; only a receipt carrying
/// every required op at a confirmed or not-applicable outcome verifies.
fn audit_receipt(
    report: &mut VerifyReport,
    path: &std::path::Path,
    name: &str,
    now: chrono::DateTime<chrono::Utc>,
    since_secs: u64,
    build: &str,
    skipped_builds: &mut std::collections::BTreeSet<String>,
) {
    report.checked += 1;
    let receipt = match read_reap_receipt(path) {
        Ok(r) => r,
        Err(err) => {
            report.problems.push(VerifyProblem {
                receipt: name.to_string(),
                reason: format!("unreadable: {err}"),
            });
            return;
        }
    };
    let Some(reaped) = chrono::DateTime::parse_from_rfc3339(&receipt.reaped_at)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
    else {
        report.problems.push(VerifyProblem {
            receipt: name.to_string(),
            reason: "reaped_at missing or unparseable".into(),
        });
        return;
    };
    if (now - reaped).num_seconds() > since_secs as i64 {
        return; // outside the window: not this report's population
    }
    // A removal receipt (`removed_by` set, the x-b150 shape) records a
    // deliberate operator removal, not a reap: it carries no effect
    // records by contract, so demanding them here would make one plain
    // `fno agents rm` red the whole window.
    if receipt.removed_by.is_some() {
        return; // removal receipt: not a retirement, not this audit's population
    }
    let stamped = receipt.writer_build.as_deref().unwrap_or_default();
    if stamped != build {
        skipped_builds.insert(if stamped.is_empty() {
            "(unstamped)".to_string()
        } else {
            stamped.to_string()
        });
        report.skipped.push(VerifyProblem {
            receipt: name.to_string(),
            reason: format!(
                "stale build: receipt written by {:?}, this verifier is {build:?} (a dry run or an older daemon never counts)",
                stamped
            ),
        });
        return;
    }
    // The gate (x-5aef task 2.1): every required op must be present and
    // confirmed (or measured not-applicable). A missing op names itself and
    // what the receipt carries, so the refusal is evidence, not a bare red.
    let mut missing: Vec<&str> = Vec::new();
    for required in REQUIRED_OPS {
        let rec = receipt.effects.iter().find(|e| e.op == required);
        match rec {
            None => missing.push(required),
            Some(e) if !outcome_confirmed(&e.outcome) => {
                report.problems.push(VerifyProblem {
                    receipt: name.to_string(),
                    reason: format!(
                        "required effect op {required:?} carries unconfirmed outcome {:?}",
                        e.outcome
                    ),
                });
                return;
            }
            Some(_) => {}
        }
    }
    if !missing.is_empty() {
        let carried: Vec<&str> = receipt.effects.iter().map(|e| e.op.as_str()).collect();
        report.problems.push(VerifyProblem {
            receipt: name.to_string(),
            reason: format!(
                "required effect op(s) {} absent (this receipt carries: {}). Note: mux effects are not required here; no fno-agents call site emits one yet (owner: x-7649).",
                missing.iter().map(|o| format!("{o:?}")).collect::<Vec<_>>().join(", "),
                carried.join(", ")
            ),
        });
        return;
    }
    // Any EXTRA op beyond the required set must still be confirmed: a
    // partial retirement is never readable as a full one.
    let unconfirmed: Vec<&str> = receipt
        .effects
        .iter()
        .map(|e| e.outcome.as_str())
        .filter(|o| !outcome_confirmed(o))
        .collect();
    if !unconfirmed.is_empty() {
        report.problems.push(VerifyProblem {
            receipt: name.to_string(),
            reason: format!("unconfirmed effects: {}", unconfirmed.join(", ")),
        });
        return;
    }
    report.verified.push(VerifiedRetirement {
        harness: receipt.harness,
        session_id: receipt.harness_session_id,
        row_name: receipt.row_name,
        reaped_at: receipt.reaped_at,
        writer_build: build.to_string(),
        effects: receipt
            .effects
            .iter()
            .map(|e| format!("{}={}", e.op, e.outcome))
            .collect(),
    });
}

/// The cohort gate (AC2): every named session must appear among the
/// verified retirements, by session id, case-insensitively. Absent
/// expectations contribute nothing; each missing name is a problem.
fn audit_cohort(report: &mut VerifyReport) {
    for expected in &report.expected {
        let found = report
            .verified
            .iter()
            .any(|v| v.session_id.eq_ignore_ascii_case(expected));
        if !found {
            report.missing.push(expected.clone());
            report.problems.push(VerifyProblem {
                receipt: "cohort".into(),
                reason: format!(
                    "expected session {expected:?} has no verified retirement in this window"
                ),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt::{build_reap_receipt, write_reap_receipt, EffectRecord, ReapReceipt};
    use crate::state;

    fn temp_home() -> AgentsHome {
        // A per-call counter, not the clock alone: cargo runs these tests as
        // threads of one process, and two of them reading the same coarse
        // nanosecond shared a home - one test's receipts then landed in
        // another's window and reddened it at random.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "gc-verify-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let home = AgentsHome::at(dir);
        home.ensure_root().unwrap();
        home
    }

    fn row(name: &str) -> state::RegistryEntry {
        serde_json::from_str(&format!(
            r#"{{"name":"{name}","short_id":"{name}","harness":"codex","harness_session_id":"sess-{name}","cwd":"/tmp","created_at":"2026-09-01T00:00:00Z","status":"live"}}"#
        ))
        .unwrap()
    }

    fn stamp(receipt: &mut ReapReceipt, build: Option<&str>) {
        receipt.writer_build = build.map(str::to_string);
        receipt.reaped_at = crate::daemon::now_rfc3339_like();
    }

    fn effect(op: &str, outcome: &str) -> EffectRecord {
        EffectRecord {
            op: op.into(),
            outcome: outcome.into(),
            detail: None,
            at: crate::daemon::now_rfc3339_like(),
        }
    }

    /// The full required op set, all confirmed: the shape a real retirement
    /// stages since x-5aef task 1.1.
    fn confirmed_effects() -> Vec<EffectRecord> {
        vec![
            effect("native-stop", "confirmed-removed"),
            effect("active-surface", "confirmed-removed"),
            effect("resume-evidence", "confirmed-removed"),
        ]
    }

    /// The auditor's shape: ONE op (the only op the pre-x-5aef producer
    /// ever wrote). Stale/out-of-window fixtures carry it because their
    /// content never reaches the effects checks.
    fn confirmed_effect() -> EffectRecord {
        effect("active-surface", "confirmed-removed")
    }

    #[test]
    fn an_empty_store_fails_the_audit() {
        // AC11-EDGE: the verifier must fail before its evidence exists - an
        // empty window is no evidence, not a clean bill.
        let home = temp_home();
        let report = verify(&home, 24 * 3600, &[]);
        assert!(!report.passes());
        assert_eq!(report.checked, 0);
        assert!(
            report.problems[0].reason.contains("store absent"),
            "{:?}",
            report.problems
        );
    }

    #[test]
    fn a_stale_build_never_passes_the_audit() {
        // A receipt stamped by another binary can never verify. It skips -
        // no PER-RECEIPT refusal, because any rollout holds both builds'
        // receipts - and the audit fails on empty evidence, with the window
        // itself naming why.
        let home = temp_home();
        let mut receipt = build_reap_receipt(&row("old"), None).unwrap();
        stamp(&mut receipt, Some("fno-agents 0.0.1"));
        receipt.effects = confirmed_effects();
        write_reap_receipt(&home, &receipt).unwrap();

        let report = verify(&home, 24 * 3600, &[]);
        assert!(!report.passes());
        assert!(report.verified.is_empty());
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        assert!(
            report.problems[0]
                .reason
                .contains("no retirement by the current build"),
            "{:?}",
            report.problems
        );
        // The window's own reason names the writer build and the remedy -
        // the only thing an operator reads when this is red.
        assert!(
            report.problems[0].reason.contains("fno-agents 0.0.1"),
            "{:?}",
            report.problems
        );
        assert!(
            report.problems[0].reason.contains("fno doctor update"),
            "{:?}",
            report.problems
        );
        assert!(
            report
                .skipped
                .iter()
                .any(|p| p.reason.contains("stale build")),
            "{:?}",
            report.skipped
        );
    }

    #[test]
    fn an_unstamped_v1_receipt_reads_stale() {
        // Pre-stamp receipts (writer_build absent) are the deployed fleet's
        // own history: skip, never verify, never refuse PER RECEIPT. The
        // window itself still says why it is red - a report with nothing in
        // `problems` and nothing in `verified` names no reason at all.
        let home = temp_home();
        let mut receipt = build_reap_receipt(&row("v1"), None).unwrap();
        stamp(&mut receipt, None);
        receipt.effects = confirmed_effects();
        write_reap_receipt(&home, &receipt).unwrap();

        let report = verify(&home, 24 * 3600, &[]);
        assert!(!report.passes());
        assert!(report.skipped.len() == 1, "{:?}", report.skipped);
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        assert!(
            report.problems[0].reason.contains("1 from another build"),
            "{:?}",
            report.problems
        );
    }

    #[test]
    fn a_current_build_with_confirmed_effects_passes() {
        // AC11-HP: current-build receipts naming the matched identities,
        // build and confirmed effects - and the audit passes.
        let home = temp_home();
        for name in ["a", "b"] {
            let mut receipt = build_reap_receipt(&row(name), None).unwrap();
            stamp(&mut receipt, Some(current_build().as_str()));
            receipt.effects = confirmed_effects();
            write_reap_receipt(&home, &receipt).unwrap();
        }
        let report = verify(&home, 24 * 3600, &[]);
        assert!(report.passes(), "{:?}", report.problems);
        assert_eq!(report.verified.len(), 2);
        assert_eq!(report.verified[0].session_id, "sess-a");
        assert!(report.verified[0]
            .effects
            .iter()
            .any(|e| e == "active-surface=confirmed-removed"));
    }

    #[test]
    fn a_rollout_window_passes_on_current_build_evidence_alone() {
        // The live rollout shape: the previous deploy's receipts sit in the
        // window beside this build's. They skip; the current-build receipt
        // verifies; the audit passes - the rollout tail never holds the
        // probe red for a full window.
        let home = temp_home();
        let mut old = build_reap_receipt(&row("prev"), None).unwrap();
        stamp(&mut old, Some("fno-agents 0.3.2 (2942a5f3b7cf, release)"));
        old.effects = vec![confirmed_effect()];
        write_reap_receipt(&home, &old).unwrap();
        let mut cur = build_reap_receipt(&row("live"), None).unwrap();
        stamp(&mut cur, Some(current_build().as_str()));
        cur.effects = confirmed_effects();
        write_reap_receipt(&home, &cur).unwrap();

        let report = verify(&home, 24 * 3600, &[]);
        assert!(report.passes(), "{:?}", report.problems);
        assert_eq!(report.verified.len(), 1);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.checked, 2);
    }

    #[test]
    fn a_partial_effect_set_fails_the_audit() {
        // AC11-EDGE: an unconfirmed effect (a kept native surface) is never
        // a pass - a partial retirement must not read as applied. The
        // receipt carries the full op set with one op unconfirmed, so the
        // refusal is the unconfirmed-outcome one, not the missing-op one.
        let home = temp_home();
        let mut receipt = build_reap_receipt(&row("part"), None).unwrap();
        stamp(&mut receipt, Some(current_build().as_str()));
        receipt.effects = vec![
            effect("native-stop", "confirmed-removed"),
            effect("active-surface", "kept"),
            effect("resume-evidence", "confirmed-removed"),
        ];
        write_reap_receipt(&home, &receipt).unwrap();
        let report = verify(&home, 24 * 3600, &[]);
        assert!(!report.passes());
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.reason.contains("unconfirmed")),
            "{:?}",
            report.problems
        );
    }

    /// x-5aef AC1-HP, the auditor's exact reproduction: one current-build
    /// v2 receipt carrying only `active-surface=confirmed-removed`, no
    /// native stop, no resume evidence, no cohort. The old gate certified
    /// it (`passes:true, verified:1`); this gate refuses and names the ops
    /// the receipt does not carry.
    #[test]
    fn ac1_hp_a_single_op_receipt_is_refused() {
        let home = temp_home();
        let mut receipt = build_reap_receipt(&row("synthetic"), None).unwrap();
        stamp(&mut receipt, Some(current_build().as_str()));
        receipt.effects = vec![confirmed_effect()];
        write_reap_receipt(&home, &receipt).unwrap();

        let report = verify(&home, 24 * 3600, &[]);
        assert!(!report.passes(), "{:?}", report.verified);
        assert!(report.verified.is_empty());
        let refusal = report
            .problems
            .iter()
            .find(|p| p.receipt != "window of 86400s")
            .expect("a per-receipt refusal names the missing ops");
        assert!(
            refusal.reason.contains("required effect op")
                && refusal.reason.contains("native-stop")
                && refusal.reason.contains("resume-evidence"),
            "{:?}",
            refusal
        );
        assert!(
            refusal.reason.contains("mux effects are not required here"),
            "the refusal carries the mux context: {:?}",
            refusal
        );
    }

    /// x-5aef AC1-EDGE: the full set at confirmed/not-applicable outcomes
    /// verifies; the same receipt with `resume-evidence` at `failed`
    /// refuses, naming that op.
    #[test]
    fn ac1_edge_full_set_verifies_and_a_failed_op_refuses() {
        let home = temp_home();
        let mut full = build_reap_receipt(&row("full"), None).unwrap();
        stamp(&mut full, Some(current_build().as_str()));
        full.effects = vec![
            effect("native-stop", "confirmed-removed"),
            effect("active-surface", "confirmed-already-absent"),
            effect("resume-evidence", "not-applicable"),
        ];
        write_reap_receipt(&home, &full).unwrap();
        let report = verify(&home, 24 * 3600, &[]);
        assert!(report.passes(), "{:?}", report.problems);
        assert_eq!(report.verified.len(), 1);

        let mut failed = build_reap_receipt(&row("failed"), None).unwrap();
        stamp(&mut failed, Some(current_build().as_str()));
        failed.effects = vec![
            effect("native-stop", "confirmed-removed"),
            effect("active-surface", "confirmed-removed"),
            effect("resume-evidence", "failed"),
        ];
        write_reap_receipt(&home, &failed).unwrap();
        // Re-read the SAME store: the failed receipt joins the window.
        let report = verify(&home, 24 * 3600, &[]);
        assert!(!report.passes());
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.reason.contains("resume-evidence") && p.reason.contains("failed")),
            "{:?}",
            report.problems
        );
    }

    /// x-5aef AC2-HP: a window holding a verified receipt for `a` only,
    /// with a cohort of a and b, refuses and names b.
    #[test]
    fn ac2_hp_the_cohort_names_missing_sessions() {
        let home = temp_home();
        let mut receipt = build_reap_receipt(&row("a"), None).unwrap();
        stamp(&mut receipt, Some(current_build().as_str()));
        receipt.effects = confirmed_effects();
        write_reap_receipt(&home, &receipt).unwrap();

        let expected = vec!["sess-a".to_string(), "sess-b".to_string()];
        let report = verify(&home, 24 * 3600, &expected);
        assert!(!report.passes());
        assert_eq!(report.missing, vec!["sess-b".to_string()]);
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.reason.contains("sess-b") && p.reason.contains("no verified retirement")),
            "{:?}",
            report.problems
        );
        // The report carries the cohort, so a JSON read answers who was
        // expected and who is missing without re-deriving either.
        let json = report.to_json();
        assert_eq!(json["expected"].as_array().map(|a| a.len()), Some(2));
        assert_eq!(json["missing"].as_array().map(|a| a.len()), Some(1));
    }

    /// x-5aef AC2-EDGE: no `--expect-sessions`, the cohort check
    /// contributes nothing and the pass predicate is unchanged.
    #[test]
    fn ac2_edge_without_expectations_the_gate_is_unchanged() {
        let home = temp_home();
        let mut receipt = build_reap_receipt(&row("a"), None).unwrap();
        stamp(&mut receipt, Some(current_build().as_str()));
        receipt.effects = confirmed_effects();
        write_reap_receipt(&home, &receipt).unwrap();
        let report = verify(&home, 24 * 3600, &[]);
        assert!(report.passes(), "{:?}", report.problems);
    }

    #[test]
    fn a_receipt_outside_the_window_is_not_audited() {
        let home = temp_home();
        let mut receipt = build_reap_receipt(&row("old"), None).unwrap();
        stamp(&mut receipt, Some("fno-agents 0.0.1"));
        receipt.effects = confirmed_effects();
        write_reap_receipt(&home, &receipt).unwrap();
        // Age the receipt past the window without rewriting it: the file's
        // reaped_at is the audit's clock.
        let path = crate::receipt::reap_receipt_path(&home, &receipt);
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        v["reaped_at"] = serde_json::Value::String(
            (chrono::Utc::now() - chrono::Duration::seconds(48 * 3600)).to_rfc3339(),
        );
        std::fs::write(&path, serde_json::to_vec_pretty(&v).unwrap()).unwrap();

        let report = verify(&home, 24 * 3600, &[]);
        // Out-of-window receipts are not this report's population: the
        // window reads empty, and an empty window never passes. It still
        // says so - the counts name which emptiness this is.
        assert!(!report.passes(), "verified must be empty");
        assert_eq!(report.checked, 1);
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        assert!(
            report.problems[0].reason.contains("1 receipt(s) read")
                && report.problems[0].reason.contains("0 from another build"),
            "{:?}",
            report.problems
        );
    }

    #[test]
    fn a_removal_receipt_is_not_this_audits_population() {
        // The x-b150 removal receipt (removed_by set, no effect records by
        // contract) must not red the window: one plain `fno agents rm` is a
        // deliberate operator removal, not a failed reap.
        let home = temp_home();
        let mut receipt = build_reap_receipt(&row("rm-row"), None).unwrap();
        stamp(&mut receipt, Some(current_build().as_str()));
        receipt.removed_by = Some("operator".into());
        write_reap_receipt(&home, &receipt).unwrap();

        let report = verify(&home, 24 * 3600, &[]);
        assert_eq!(report.checked, 1);
        assert!(report.verified.is_empty());
        assert!(
            !report.passes(),
            "no retirement evidence: an empty window is no pass"
        );
        // The removal receipt itself is never refused. The only entry is the
        // window's own line saying it found no retirement to verify.
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        assert!(
            report.problems[0]
                .reason
                .starts_with("no retirement by the current build to verify"),
            "{:?}",
            report.problems
        );
    }
}
