//! The retirement verifier: a READ-ONLY audit of the receipts store that
//! answers "did a writer that promised the current retirement contract
//! actually retire, resumably, in this window" - and refuses everything
//! else.
//!
//! A pass requires, for every receipt in the window: a contract stamp
//! naming the CURRENT retirement contract (a receipt from a writer that
//! never promised the required op set never counts), the FULL required op
//! set (`native-stop`, `active-surface`, `resume-evidence`, `mux-member`,
//! each at a confirmed or measured not-applicable outcome), and nothing
//! unconfirmed. An empty store, an unreadable receipt, a partial effect
//! set: each is a named refusal with a nonzero exit - this verb must fail
//! before its evidence exists, because it is the plan's done probe. The
//! required set is what closes the synthetic-receipt hole: the auditor's
//! receipt carried one op and the gate certified it; a pass now means the
//! promised outcome, not a nonempty list.
//!
//! The gate also derives its cohort: every in-window
//! `agent_row_reaped` event that does not carry `receipt_staged` names a
//! session that must have a receipt file on disk, checked by existence, so
//! a retirement that dropped a row without persisting its receipt is
//! visible without anyone passing `--expect-sessions`.
//!
//! The pin is the CONTRACT the writer promised, not its build. The build
//! moves on every merge that touches `crates/` (post-merge sync reinstalls
//! after each merge), so a build pin reads a correct retirement as stale
//! whenever an unrelated merge lands between the write and the audit. A
//! receipt on another contract (including one written before the stamp
//! existed) SKIPS: it can never verify, and it must not rebrand the
//! fleet's rollout tail as failure - during any rollout the window holds
//! both contracts' receipts, and those refusals would hold the probe red
//! for a full window. Only current-contract receipts enter `verified`, so
//! a window of nothing but other-contract receipts still fails on empty
//! evidence. The writer build stays on the receipt and in the report as
//! provenance.

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
    /// In-window `agent_row_reaped` events the derived cohort examined
    /// A zero names itself, so "checked the log, found nothing"
    /// never reads as "the log was not read".
    pub reaped_events: usize,
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
            "reaped_events": self.reaped_events,
            "build": current_build(),
            "contract": retirement_contract(),
        })
    }
}

/// The identity of the BUILD that wrote a receipt: provenance, not the
/// audit pin. The pin is `retirement_contract`.
///
/// The writer and the verifier are never the same file. `fno-agents-daemon`
/// writes the receipt; `fno-agents` runs `reap --verify`. One `cargo install`
/// lays down three separate executables, so any identity read off the
/// running exe (its path, its mtime, its own hash) differs between writer
/// and reader. The build sidesteps that: it is what the whole triad bakes
/// at compile time - the crates/ subtree rev from build.rs, the same
/// quantity `fno doctor update` already uses to prove the three bins are
/// ONE build. Two deployments of the same package version still differ
/// whenever the source moved. A rebuild of identical source shares a
/// stamp, which is correct - it is the same build. The `-dirty` suffix
/// names an uncommitted tree so a dev build never passes itself off as
/// the committed rev.
///
/// The known limit: a build with no git checkout to read (a crates.io
/// tarball) bakes the rev `unknown`, so every such build of one package
/// version shares a stamp. Provenance then reads at version granularity
/// there. That is the weaker end of the trade, and it is the end that
/// still works.
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

/// The ops every verified retirement must carry: a pass means the promised
/// outcome, not a nonempty list. The
/// auditor's synthetic receipt carried `active-surface` alone and the old
/// nonempty check certified it. `resume-evidence` is what makes the receipt
/// a recovery record instead of an obituary: absent or failed, the gate
/// refuses. `mux-member` is the squad-store half: a session still
/// held in the shared mux store is not retired, and `not-applicable` is the
/// measured answer for a row with no membership.
pub const REQUIRED_OPS: [&str; 4] = [
    "native-stop",
    "active-surface",
    "resume-evidence",
    "mux-member",
];

/// The retirement contract a receipt's writer promised: the required op
/// set. Derived from `REQUIRED_OPS`, so adding an op changes the stamp in
/// the same commit and receipts written before it skip instead of
/// refusing.
pub fn retirement_contract() -> String {
    REQUIRED_OPS.join(",")
}

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
        // An absent store verifies nothing, so a named cohort is entirely
        // missing: name it, or the JSON reads expected-without-missing and
        // a consumer re-derives the gap the gate already knows.
        audit_cohort(&mut report);
        audit_event_cohort(&mut report, home, chrono::Utc::now(), since_secs);
        return report;
    };
    let now = chrono::Utc::now();
    let contract = retirement_contract();
    let mut skipped_writers: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
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
            contract.as_str(),
            &mut skipped_writers,
        );
    }
    // A window that verified nothing and refused nothing reads as a silent
    // red: `problems` empty, `passes` false, and no reason on the page. Name
    // the shape, with the counts that separate its causes. On the live fleet
    // this exact output (523 checked, 0 verified, 0 problems) hid a pin
    // that could never match, and reading it took a source dive.
    if report.verified.is_empty() && report.problems.is_empty() {
        // Every in-window receipt read here was written on another
        // contract: name it and the remedy, since this string is the only
        // thing an operator reads when the window is red. Deploying still
        // fixes it - a writer daemon on an older contract is an older
        // build.
        let remedy = if skipped_writers.is_empty() {
            String::new()
        } else {
            format!(
                ". Written by: {}. Run `fno doctor update` to deploy the current contract ({contract:?}).",
                skipped_writers.into_iter().collect::<Vec<_>>().join(", ")
            )
        };
        report.problems.push(VerifyProblem {
            receipt: format!("window of {since_secs}s"),
            reason: format!(
                "no retirement on the current contract to verify: {} receipt(s) read, \
                 {} on another contract, none stamped {contract:?}{remedy}",
                report.checked,
                report.skipped.len()
            ),
        });
    }
    audit_cohort(&mut report);
    audit_event_cohort(&mut report, home, now, since_secs);
    report
}

/// Audit one receipt: window, population, contract stamp, and the
/// required op set. Each unmet condition is a named refusal; only a
/// receipt stamped with the current contract and carrying every required
/// op at a confirmed or not-applicable outcome verifies.
fn audit_receipt(
    report: &mut VerifyReport,
    path: &std::path::Path,
    name: &str,
    now: chrono::DateTime<chrono::Utc>,
    since_secs: u64,
    contract: &str,
    skipped_writers: &mut std::collections::BTreeSet<String>,
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
    // The sweep is the only writer that runs the four retirement effects,
    // so it is the only writer this audit has an opinion about. Any OTHER
    // non-empty stamp (roster-reap, the argv verb of an `rm` door) is a
    // removal receipt and skips, exactly as before. An EMPTY stamp is the
    // pre-stamp tail (every receipt written before `removed_by` was
    // required) and stays in the population until the retention window
    // rolls it out.
    if !receipt.removed_by.is_empty()
        && receipt.removed_by != crate::receipt::Writer::GcSweep.surface()
    {
        return;
    }
    // The pin is the contract the writer promised, never its build: the
    // build moves on every merge that touches crates/, and a build pin
    // would read a correct retirement as stale after any unrelated
    // reinstall. A writer that promised a different contract (or none -
    // every pre-stamp receipt) skips as the rollout tail.
    let writer_build = receipt
        .writer_build
        .clone()
        .unwrap_or_else(|| "(unstamped)".into());
    let stamped = receipt.retirement_contract.as_deref().unwrap_or_default();
    if stamped != contract {
        skipped_writers.insert(writer_build.clone());
        report.skipped.push(VerifyProblem {
            receipt: name.to_string(),
            reason: format!(
                "other contract: receipt promised {:?} (written by {writer_build:?}), this verifier requires {contract:?}",
                if stamped.is_empty() { "(unstamped)" } else { stamped }
            ),
        });
        return;
    }
    // The gate: every required op must be present and confirmed (or
    // measured not-applicable). A missing op names itself and what the
    // receipt carries, so the refusal is evidence, not a bare red.
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
                "required effect op(s) {} absent (this receipt carries: {})",
                missing
                    .iter()
                    .map(|o| format!("{o:?}"))
                    .collect::<Vec<_>>()
                    .join(", "),
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
        writer_build,
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

/// The derived cohort, always on: every in-window
/// `agent_row_reaped` event whose door did not stamp `receipt_staged`
/// (roster-reap stamps it; the sweep and merge doors write the receipt
/// before the row drop) names a session that must have a receipt FILE on
/// disk. Existence is the test, so a stale-build receipt is accounted for
/// and only a genuinely lost receipt reddens. An absent log contributes
/// nothing; an unread log is not an empty one.
fn audit_event_cohort(
    report: &mut VerifyReport,
    home: &AgentsHome,
    now: chrono::DateTime<chrono::Utc>,
    since_secs: u64,
) {
    let active = home.events_jsonl();
    let rotated = crate::events::rotated_path(&active);
    for file in [rotated, active] {
        let raw = match std::fs::read_to_string(&file) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                report.problems.push(VerifyProblem {
                    receipt: file.to_string_lossy().to_string(),
                    reason: format!("events log unreadable: {err}"),
                });
                continue;
            }
        };
        for line in raw.lines() {
            let Ok(event) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if event.get("type").and_then(Value::as_str) != Some("agent_row_reaped") {
                continue;
            }
            let Some(ts) = event
                .get("ts")
                .and_then(Value::as_str)
                .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc))
            else {
                continue;
            };
            if (now - ts).num_seconds() > since_secs as i64 {
                continue; // outside the window: not this report's population
            }
            report.reaped_events += 1;
            let Some(data) = event.get("data") else {
                continue;
            };
            if data.get("receipt_staged").is_some() {
                continue; // roster-reap door: carries its own accounting
            }
            let harness = data.get("harness").and_then(Value::as_str).unwrap_or("");
            let session_id = data
                .get("harness_session_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            if harness.is_empty() || session_id.is_empty() {
                continue;
            }
            let name = data.get("name").and_then(Value::as_str).unwrap_or("");
            if !crate::receipt::reap_receipt_path_for(home, harness, session_id).exists() {
                report.problems.push(VerifyProblem {
                    receipt: "events".into(),
                    reason: format!(
                        "session {harness}:{session_id} ({name}) reaped at {ts} with no receipt on disk"
                    ),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt::{
        build_reap_receipt, write_reap_receipt, EffectRecord, ReapReceipt, Writer,
    };
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

    fn stamp(receipt: &mut ReapReceipt, contract: Option<&str>) {
        receipt.retirement_contract = contract.map(str::to_string);
        // The writer build is always present: it is provenance, not the pin.
        receipt.writer_build = Some(current_build());
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
    /// stages since the mux-member effect landed (the op included).
    fn confirmed_effects() -> Vec<EffectRecord> {
        vec![
            effect("native-stop", "confirmed-removed"),
            effect("active-surface", "confirmed-removed"),
            effect("mux-member", "confirmed-removed"),
            effect("resume-evidence", "confirmed-removed"),
        ]
    }

    /// The auditor's shape: ONE op (the only op the pre-gate producer
    /// ever wrote). Other-contract/out-of-window fixtures carry it because their
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

    /// A named cohort against an ABSENT store verifies nothing, so every
    /// expected session is missing: the JSON must say so instead of reading
    /// expected-without-missing over a store-absent refusal.
    #[test]
    fn an_absent_store_names_the_whole_cohort_missing() {
        let home = temp_home();
        let expected = vec!["sess-a".to_string(), "sess-b".to_string()];
        let report = verify(&home, 24 * 3600, &expected);
        assert!(!report.passes());
        assert_eq!(report.missing, expected);
    }

    #[test]
    fn an_older_contract_never_passes_the_audit() {
        // A receipt promising a different contract can never verify. It
        // skips - no PER-RECEIPT refusal, because any rollout holds both
        // contracts' receipts - and the audit fails on empty evidence,
        // with the window itself naming why.
        let home = temp_home();
        let mut receipt = build_reap_receipt(&row("old"), None, Writer::GcSweep).unwrap();
        stamp(
            &mut receipt,
            Some("native-stop,active-surface,resume-evidence"),
        );
        receipt.effects = confirmed_effects();
        write_reap_receipt(&home, &receipt).unwrap();

        let report = verify(&home, 24 * 3600, &[]);
        assert!(!report.passes());
        assert!(report.verified.is_empty());
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        assert!(
            report.problems[0]
                .reason
                .contains("no retirement on the current contract"),
            "{:?}",
            report.problems
        );
        // The window's own reason names the writer builds and the remedy -
        // the only thing an operator reads when this is red.
        assert!(
            report.problems[0].reason.contains(&current_build()),
            "{:?}",
            report.problems
        );
        assert!(
            report.problems[0].reason.contains("fno doctor update"),
            "{:?}",
            report.problems
        );
        // The skip names the promised contract and the writer build.
        assert!(
            report
                .skipped
                .iter()
                .any(|p| p.reason.contains("other contract")
                    && p.reason
                        .contains("native-stop,active-surface,resume-evidence")
                    && p.reason.contains(&current_build())),
            "{:?}",
            report.skipped
        );
    }

    #[test]
    fn an_unstamped_v1_receipt_reads_stale() {
        // Pre-stamp receipts (retirement_contract absent) are the deployed
        // fleet's own history: skip, never verify, never refuse PER
        // RECEIPT. The window itself still says why it is red - a report
        // with nothing in `problems` and nothing in `verified` names no
        // reason at all.
        let home = temp_home();
        let mut receipt = build_reap_receipt(&row("v1"), None, Writer::GcSweep).unwrap();
        stamp(&mut receipt, None);
        receipt.effects = confirmed_effects();
        write_reap_receipt(&home, &receipt).unwrap();

        let report = verify(&home, 24 * 3600, &[]);
        assert!(!report.passes());
        assert!(report.skipped.len() == 1, "{:?}", report.skipped);
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        assert!(
            report.problems[0].reason.contains("1 on another contract"),
            "{:?}",
            report.problems
        );
    }

    #[test]
    fn a_current_contract_with_confirmed_effects_passes() {
        // Current-contract receipts naming the matched identities, writer
        // build and confirmed effects - and the audit passes.
        let home = temp_home();
        for name in ["a", "b"] {
            let mut receipt = build_reap_receipt(&row(name), None, Writer::GcSweep).unwrap();
            stamp(&mut receipt, Some(retirement_contract().as_str()));
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
    fn a_rollout_window_passes_on_current_contract_evidence_alone() {
        // The live rollout shape: receipts written before an op was added
        // sit in the window beside current-contract receipts. They skip;
        // the current-contract receipt verifies; the audit passes - the
        // rollout tail never holds the probe red for a full window.
        let home = temp_home();
        let mut old = build_reap_receipt(&row("prev"), None, Writer::GcSweep).unwrap();
        stamp(&mut old, Some("native-stop,active-surface,resume-evidence"));
        old.effects = vec![confirmed_effect()];
        write_reap_receipt(&home, &old).unwrap();
        let mut cur = build_reap_receipt(&row("live"), None, Writer::GcSweep).unwrap();
        stamp(&mut cur, Some(retirement_contract().as_str()));
        cur.effects = confirmed_effects();
        write_reap_receipt(&home, &cur).unwrap();

        let report = verify(&home, 24 * 3600, &[]);
        assert!(report.passes(), "{:?}", report.problems);
        assert_eq!(report.verified.len(), 1);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.checked, 2);
    }

    /// The done probe, in unit form: a receipt written by an older build
    /// on the CURRENT contract verifies, and the verified row carries
    /// that older build as provenance. A merge that reinstalls the
    /// binary between the write and the audit no longer reds the probe.
    #[test]
    fn an_older_build_on_the_current_contract_verifies() {
        let home = temp_home();
        let mut receipt = build_reap_receipt(&row("older"), None, Writer::GcSweep).unwrap();
        stamp(&mut receipt, Some(retirement_contract().as_str()));
        receipt.writer_build =
            Some("fno-agents 0.3.2 rev cf7e0875703610d488e3ee2b2bdecdfc3f39fdb0".into());
        receipt.effects = confirmed_effects();
        write_reap_receipt(&home, &receipt).unwrap();

        let report = verify(&home, 24 * 3600, &[]);
        assert!(report.passes(), "{:?}", report.problems);
        assert_eq!(
            report.verified[0].writer_build,
            "fno-agents 0.3.2 rev cf7e0875703610d488e3ee2b2bdecdfc3f39fdb0"
        );
    }

    /// The promise is the contract, so an older build on the current
    /// contract is still fully audited: drop the mux-member op and the
    /// audit refuses, naming it.
    #[test]
    fn an_older_build_on_the_current_contract_without_mux_member_refuses() {
        let home = temp_home();
        let mut receipt = build_reap_receipt(&row("older"), None, Writer::GcSweep).unwrap();
        stamp(&mut receipt, Some(retirement_contract().as_str()));
        receipt.writer_build =
            Some("fno-agents 0.3.2 rev cf7e0875703610d488e3ee2b2bdecdfc3f39fdb0".into());
        receipt.effects = vec![
            effect("native-stop", "confirmed-removed"),
            effect("active-surface", "confirmed-removed"),
            effect("resume-evidence", "confirmed-removed"),
        ];
        write_reap_receipt(&home, &receipt).unwrap();

        let report = verify(&home, 24 * 3600, &[]);
        assert!(!report.passes());
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.reason.contains("mux-member")),
            "{:?}",
            report.problems
        );
    }

    #[test]
    fn a_partial_effect_set_fails_the_audit() {
        // AC11-EDGE: an unconfirmed effect (a kept native surface) is never
        // a pass - a partial retirement must not read as applied. The
        // receipt carries the full op set with one op unconfirmed, so the
        // refusal is the unconfirmed-outcome one, not the missing-op one.
        let home = temp_home();
        let mut receipt = build_reap_receipt(&row("part"), None, Writer::GcSweep).unwrap();
        stamp(&mut receipt, Some(retirement_contract().as_str()));
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

    /// The auditor's exact reproduction: one current-contract
    /// v2 receipt carrying only `active-surface=confirmed-removed`, no
    /// native stop, no resume evidence, no cohort. The old gate certified
    /// it (`passes:true, verified:1`); this gate refuses and names the ops
    /// the receipt does not carry.
    #[test]
    fn ac1_hp_a_single_op_receipt_is_refused() {
        let home = temp_home();
        let mut receipt = build_reap_receipt(&row("synthetic"), None, Writer::GcSweep).unwrap();
        stamp(&mut receipt, Some(retirement_contract().as_str()));
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
            refusal.reason.contains("mux-member"),
            "the refusal names the missing mux op: {:?}",
            refusal
        );
    }

    /// The full set at confirmed/not-applicable outcomes
    /// verifies; the same receipt with `resume-evidence` at `failed`
    /// refuses, naming that op.
    #[test]
    fn ac1_edge_full_set_verifies_and_a_failed_op_refuses() {
        let home = temp_home();
        let mut full = build_reap_receipt(&row("full"), None, Writer::GcSweep).unwrap();
        stamp(&mut full, Some(retirement_contract().as_str()));
        full.effects = vec![
            effect("native-stop", "confirmed-removed"),
            effect("active-surface", "confirmed-already-absent"),
            effect("mux-member", "not-applicable"),
            effect("resume-evidence", "not-applicable"),
        ];
        write_reap_receipt(&home, &full).unwrap();
        let report = verify(&home, 24 * 3600, &[]);
        assert!(report.passes(), "{:?}", report.problems);
        assert_eq!(report.verified.len(), 1);

        let mut failed = build_reap_receipt(&row("failed"), None, Writer::GcSweep).unwrap();
        stamp(&mut failed, Some(retirement_contract().as_str()));
        failed.effects = vec![
            effect("native-stop", "confirmed-removed"),
            effect("active-surface", "confirmed-removed"),
            effect("mux-member", "confirmed-removed"),
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

    /// AC2-HP: a current-contract receipt carrying the pre-gate op set
    /// (everything but mux-member) refuses, names the missing op.
    #[test]
    fn ac2_hp_a_receipt_without_the_mux_member_op_refuses() {
        let home = temp_home();
        let mut receipt = build_reap_receipt(&row("premux"), None, Writer::GcSweep).unwrap();
        stamp(&mut receipt, Some(retirement_contract().as_str()));
        receipt.effects = vec![
            effect("native-stop", "confirmed-removed"),
            effect("active-surface", "confirmed-removed"),
            effect("resume-evidence", "confirmed-removed"),
        ];
        write_reap_receipt(&home, &receipt).unwrap();

        let report = verify(&home, 24 * 3600, &[]);
        assert!(!report.passes(), "{:?}", report.verified);
        let refusal = report
            .problems
            .iter()
            .find(|p| p.receipt != "window of 86400s")
            .expect("a per-receipt refusal names the missing op");
        assert!(refusal.reason.contains("mux-member"), "{:?}", refusal);
    }

    /// AC2-EDGE: all four ops with mux-member measured
    /// not-applicable, and no events log: the gate passes.
    #[test]
    fn ac2_edge_not_applicable_mux_member_passes_with_no_events_log() {
        let home = temp_home();
        let mut receipt = build_reap_receipt(&row("nomux"), None, Writer::GcSweep).unwrap();
        stamp(&mut receipt, Some(retirement_contract().as_str()));
        receipt.effects = vec![
            effect("native-stop", "confirmed-removed"),
            effect("active-surface", "confirmed-removed"),
            effect("mux-member", "not-applicable"),
            effect("resume-evidence", "confirmed-removed"),
        ];
        write_reap_receipt(&home, &receipt).unwrap();
        let report = verify(&home, 24 * 3600, &[]);
        assert!(report.passes(), "{:?}", report.problems);
        assert_eq!(report.reaped_events, 0, "no log read: zero names itself");
    }

    /// AC3-HP: an in-window reaped event with no `receipt_staged`
    /// stamp, for a session with no receipt file, fails the gate without
    /// anyone passing `--expect-sessions`.
    #[test]
    fn ac3_hp_a_reaped_event_without_a_receipt_fails_the_gate() {
        let home = temp_home();
        write_event(
            &home,
            &serde_json::json!({
                "name": "row-a",
                "harness": "codex",
                "harness_session_id": "sess-lost",
            }),
        );
        // A live unrelated receipt so the window is not empty-shaped: the
        // assertion targets the events problem specifically.
        let mut receipt = build_reap_receipt(&row("live"), None, Writer::GcSweep).unwrap();
        stamp(&mut receipt, Some(retirement_contract().as_str()));
        receipt.effects = confirmed_effects();
        write_reap_receipt(&home, &receipt).unwrap();

        let report = verify(&home, 24 * 3600, &[]);
        assert!(!report.passes(), "{:?}", report.problems);
        let problem = report
            .problems
            .iter()
            .find(|p| p.receipt == "events")
            .expect("the derived cohort names the lost receipt");
        assert!(
            problem.reason.contains("codex:sess-lost")
                && problem.reason.contains("row-a")
                && problem.reason.contains("no receipt on disk"),
            "{:?}",
            problem
        );
        assert_eq!(report.reaped_events, 1);
    }

    /// AC3-EDGE: a `receipt_staged` event (roster-reap), an
    /// out-of-window event, and an event whose receipt exists but is
    /// stale-build add no derived-cohort problem.
    #[test]
    fn ac3_edge_stamped_out_of_window_and_stale_receipt_events_add_nothing() {
        let home = temp_home();
        // roster-reap door: carries receipt_staged.
        write_event(
            &home,
            &serde_json::json!({
                "name": "stamped-row",
                "harness": "codex",
                "harness_session_id": "sess-stamped",
                "receipt_staged": true,
            }),
        );
        // Outside the window.
        let old_ts = (chrono::Utc::now() - chrono::Duration::seconds(48 * 3600)).to_rfc3339();
        write_event_at(
            &home,
            &old_ts,
            &serde_json::json!({
                "name": "old-row",
                "harness": "codex",
                "harness_session_id": "sess-old",
            }),
        );
        // A stale-build receipt on disk: existence satisfies the cohort.
        let mut stale = build_reap_receipt(&row("staleholder"), None, Writer::GcSweep).unwrap();
        stamp(
            &mut stale,
            Some("native-stop,active-surface,resume-evidence"),
        );
        stale.effects = confirmed_effects();
        write_reap_receipt(&home, &stale).unwrap();
        write_event(
            &home,
            &serde_json::json!({
                "name": "stale-row",
                "harness": "codex",
                "harness_session_id": "sess-staleholder",
            }),
        );

        // A verified receipt keeps the rest of the report green so any
        // failure is attributable to the derived cohort.
        let mut receipt = build_reap_receipt(&row("live"), None, Writer::GcSweep).unwrap();
        stamp(&mut receipt, Some(retirement_contract().as_str()));
        receipt.effects = confirmed_effects();
        write_reap_receipt(&home, &receipt).unwrap();

        let report = verify(&home, 24 * 3600, &[]);
        assert!(report.passes(), "{:?}", report.problems);
        // Both in-window events were examined; only their disposition
        // spared them.
        assert_eq!(report.reaped_events, 2);
    }

    /// The test event writer: the unified envelope shape (`ts`, `type`,
    /// `source`, `data`), appended to the home's events log.
    fn write_event(home: &AgentsHome, data: &serde_json::Value) {
        write_event_at(home, &chrono::Utc::now().to_rfc3339(), data);
    }

    fn write_event_at(home: &AgentsHome, ts: &str, data: &serde_json::Value) {
        use std::io::Write;
        let line = serde_json::json!({
            "ts": ts,
            "type": "agent_row_reaped",
            "source": "daemon",
            "data": data,
        });
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(home.events_jsonl())
            .unwrap();
        writeln!(f, "{line}").unwrap();
    }

    /// A window holding a verified receipt for `a` only,
    /// with a cohort of a and b, refuses and names b.
    #[test]
    fn ac2_hp_the_cohort_names_missing_sessions() {
        let home = temp_home();
        let mut receipt = build_reap_receipt(&row("a"), None, Writer::GcSweep).unwrap();
        stamp(&mut receipt, Some(retirement_contract().as_str()));
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

    /// No `--expect-sessions`, the cohort check
    /// contributes nothing and the pass predicate is unchanged.
    #[test]
    fn ac2_edge_without_expectations_the_gate_is_unchanged() {
        let home = temp_home();
        let mut receipt = build_reap_receipt(&row("a"), None, Writer::GcSweep).unwrap();
        stamp(&mut receipt, Some(retirement_contract().as_str()));
        receipt.effects = confirmed_effects();
        write_reap_receipt(&home, &receipt).unwrap();
        let report = verify(&home, 24 * 3600, &[]);
        assert!(report.passes(), "{:?}", report.problems);
    }

    #[test]
    fn a_receipt_outside_the_window_is_not_audited() {
        let home = temp_home();
        let mut receipt = build_reap_receipt(&row("old"), None, Writer::GcSweep).unwrap();
        stamp(
            &mut receipt,
            Some("native-stop,active-surface,resume-evidence"),
        );
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
                && report.problems[0].reason.contains("0 on another contract"),
            "{:?}",
            report.problems
        );
    }

    #[test]
    fn a_removal_receipt_is_not_this_audits_population() {
        // The removal receipt (removed_by set, no effect records by
        // contract) must not red the window: one plain `fno agents rm` is a
        // deliberate operator removal, not a failed reap.
        let home = temp_home();
        let mut receipt = build_reap_receipt(&row("rm-row"), None, Writer::GcSweep).unwrap();
        stamp(&mut receipt, Some(retirement_contract().as_str()));
        receipt.removed_by = "operator".into();
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
                .starts_with("no retirement on the current contract to verify"),
            "{:?}",
            report.problems
        );
    }
}
