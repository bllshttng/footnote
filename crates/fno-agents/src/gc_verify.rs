//! The retirement verifier (x-70e1 task 5): a READ-ONLY audit of the
//! receipts store that answers "did the currently deployed build actually
//! retire, resumably, in this window" - and refuses everything else.
//!
//! A pass requires, for every receipt in the window: a build stamp naming
//! the CURRENT binary (a dry run, an older daemon, a fixture run never
//! counts), a non-empty per-effect record set, and every applicable effect
//! positively confirmed (or measured not-applicable). An empty store, an
//! unreadable receipt, a partial effect set: each is a named refusal with
//! a nonzero exit - this verb must fail before its evidence exists, because
//! it is the plan's done probe.
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

/// Audit the receipts store over the window. Read-only: nothing here writes.
pub fn verify(home: &AgentsHome, since_secs: u64) -> VerifyReport {
    let mut report = VerifyReport::default();
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
        report.checked += 1;
        let receipt = match read_reap_receipt(&path) {
            Ok(r) => r,
            Err(err) => {
                report.problems.push(VerifyProblem {
                    receipt: name,
                    reason: format!("unreadable: {err}"),
                });
                continue;
            }
        };
        let Some(reaped) = chrono::DateTime::parse_from_rfc3339(&receipt.reaped_at)
            .ok()
            .map(|dt| dt.with_timezone(&chrono::Utc))
        else {
            report.problems.push(VerifyProblem {
                receipt: name,
                reason: "reaped_at missing or unparseable".into(),
            });
            continue;
        };
        if (now - reaped).num_seconds() > since_secs as i64 {
            continue; // outside the window: not this report's population
        }
        // A removal receipt (`removed_by` set, the x-b150 shape) records a
        // deliberate operator removal, not a reap: it carries no effect
        // records by contract, so demanding them here would make one plain
        // `fno agents rm` red the whole window.
        if receipt.removed_by.is_some() {
            continue; // removal receipt: not a retirement, not this audit's population
        }
        let stamped = receipt.writer_build.as_deref().unwrap_or_default();
        if stamped != build {
            report.skipped.push(VerifyProblem {
                receipt: name,
                reason: format!(
                    "stale build: receipt written by {:?}, this verifier is {build:?} (a dry run or an older daemon never counts)",
                    stamped
                ),
            });
            continue;
        }
        if receipt.effects.is_empty() {
            report.problems.push(VerifyProblem {
                receipt: name,
                reason: "no per-effect records: the retirement's native effects were never applied"
                    .into(),
            });
            continue;
        }
        let unconfirmed: Vec<&str> = receipt
            .effects
            .iter()
            .map(|e| e.outcome.as_str())
            .filter(|o| {
                *o != "confirmed-removed"
                    && *o != "confirmed-already-absent"
                    && *o != "not-applicable"
            })
            .collect();
        if !unconfirmed.is_empty() {
            report.problems.push(VerifyProblem {
                receipt: name,
                reason: format!("unconfirmed effects: {}", unconfirmed.join(", ")),
            });
            continue;
        }
        report.verified.push(VerifiedRetirement {
            harness: receipt.harness,
            session_id: receipt.harness_session_id,
            row_name: receipt.row_name,
            reaped_at: receipt.reaped_at,
            writer_build: build.clone(),
            effects: receipt
                .effects
                .iter()
                .map(|e| format!("{}={}", e.op, e.outcome))
                .collect(),
        });
    }
    // A window that verified nothing and refused nothing reads as a silent
    // red: `problems` empty, `passes` false, and no reason on the page. Name
    // the shape, with the counts that separate its causes. On the live fleet
    // this exact output (523 checked, 0 verified, 0 problems) hid a build pin
    // that could never match, and reading it took a source dive.
    if report.verified.is_empty() && report.problems.is_empty() {
        report.problems.push(VerifyProblem {
            receipt: format!("window of {since_secs}s"),
            reason: format!(
                "no retirement by the current build to verify: {} receipt(s) read, \
                 {} from another build, none stamped {build:?}",
                report.checked,
                report.skipped.len()
            ),
        });
    }
    report
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

    fn confirmed_effect() -> EffectRecord {
        EffectRecord {
            op: "active-surface".into(),
            outcome: "confirmed-removed".into(),
            detail: None,
            at: crate::daemon::now_rfc3339_like(),
        }
    }

    #[test]
    fn an_empty_store_fails_the_audit() {
        // AC11-EDGE: the verifier must fail before its evidence exists - an
        // empty window is no evidence, not a clean bill.
        let home = temp_home();
        let report = verify(&home, 24 * 3600);
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
        receipt.effects = vec![confirmed_effect()];
        write_reap_receipt(&home, &receipt).unwrap();

        let report = verify(&home, 24 * 3600);
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
        receipt.effects = vec![confirmed_effect()];
        write_reap_receipt(&home, &receipt).unwrap();

        let report = verify(&home, 24 * 3600);
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
            receipt.effects = vec![confirmed_effect()];
            write_reap_receipt(&home, &receipt).unwrap();
        }
        let report = verify(&home, 24 * 3600);
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
        cur.effects = vec![confirmed_effect()];
        write_reap_receipt(&home, &cur).unwrap();

        let report = verify(&home, 24 * 3600);
        assert!(report.passes(), "{:?}", report.problems);
        assert_eq!(report.verified.len(), 1);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.checked, 2);
    }

    #[test]
    fn a_partial_effect_set_fails_the_audit() {
        // AC11-EDGE: an unconfirmed effect (a kept native surface) is never
        // a pass - a partial retirement must not read as applied.
        let home = temp_home();
        let mut receipt = build_reap_receipt(&row("part"), None).unwrap();
        stamp(&mut receipt, Some(current_build().as_str()));
        receipt.effects = vec![EffectRecord {
            op: "active-surface".into(),
            outcome: "kept".into(),
            detail: Some("archive op unverified".into()),
            at: crate::daemon::now_rfc3339_like(),
        }];
        write_reap_receipt(&home, &receipt).unwrap();
        let report = verify(&home, 24 * 3600);
        assert!(!report.passes());
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.reason.contains("unconfirmed effects")),
            "{:?}",
            report.problems
        );
    }

    #[test]
    fn a_receipt_outside_the_window_is_not_audited() {
        let home = temp_home();
        let mut receipt = build_reap_receipt(&row("old"), None).unwrap();
        stamp(&mut receipt, Some("fno-agents 0.0.1"));
        receipt.effects = vec![confirmed_effect()];
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

        let report = verify(&home, 24 * 3600);
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

        let report = verify(&home, 24 * 3600);
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
