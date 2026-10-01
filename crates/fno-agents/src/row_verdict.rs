//! The one fno-first verdict door for "is this registry row live?".
//!
//! fno holds the provenance: its registry row, inside-leg report, and pid
//! probe decide. A vendor surface (`claude agents --json`, an app-server
//! listing) is a check and balance - it may turn an Unknown into a verdict
//! and may be recorded disagreeing, but it never overrides a verdict fno
//! has already reached, and its silence never blocks fno. Harness-neutral
//! by construction: nothing here reads the row's harness name.
//!
//! Every attach, restore, liveness, and reap path routes through
//! [`fno_verdict`] + [`reconcile`] so "who decides" cannot diverge between
//! call sites. [`drift`] names the disagreement a caller turns into a drift
//! event.

// This first wave lands the door alone; its call sites land in the waves
// that follow, so nothing outside the tests reads it yet. Delete this allow
// when they wire in.
#![allow(dead_code)]

use crate::daemon::pid_is_gone;
use crate::state::{InsideLegState, RegistryEntry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RowVerdict {
    /// fno has positive evidence the session is alive; the payload names the
    /// evidence source.
    Live(&'static str),
    /// fno has positive evidence the session is finished; the payload is the
    /// human-readable reason.
    Finished(String),
    /// fno cannot decide; the payload says what was missing. A vendor word
    /// may still resolve it (see [`reconcile`]).
    Unknown(String),
}

/// Current wall clock in epoch seconds, 0 when the clock is unreadable. A 0
/// disqualifies the TTL rung below: `is_live_at(0)` would saturate any past
/// stamp to "inside TTL" and hold a stale report live (fail open).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The verdict fno's own rows prove, in evidence order: terminal registry
/// status first, then a `working` inside-leg report inside its TTL, then the
/// recorded pid probe. Anything else is Unknown - never a guess.
pub(crate) fn fno_verdict(e: &RegistryEntry) -> RowVerdict {
    let now = now_secs();
    if matches!(
        e.status,
        crate::AgentStatus::Exited | crate::AgentStatus::PermanentDead
    ) {
        return RowVerdict::Finished(format!("registry status {:?}", e.status));
    }
    if let Some(leg) = &e.inside_leg {
        if now > 0 && leg.state == InsideLegState::Working && leg.is_live_at(now) {
            return RowVerdict::Live("inside_leg");
        }
    }
    if let Some(pid) = e.pid {
        if pid_is_gone(pid) {
            return RowVerdict::Finished(format!("pid {pid} is gone (ESRCH)"));
        }
        return RowVerdict::Live("pid");
    }
    RowVerdict::Unknown(
        "no terminal status, no live inside-leg report, no recorded pid".to_string(),
    )
}

/// Fold a vendor word into fno's verdict. A decided fno verdict always wins:
/// the vendor is a check, so a disagreeing word keeps fno's answer (name the
/// disagreement with [`drift`]). Only an Unknown fno verdict may take the
/// vendor's word, and only when that word is one this door recognizes.
pub(crate) fn reconcile(fno: &RowVerdict, vendor: Option<&str>) -> RowVerdict {
    let Some(word) = vendor.map(str::trim).filter(|w| !w.is_empty()) else {
        return fno.clone();
    };
    match fno {
        RowVerdict::Unknown(why) => vendor_verdict(word).unwrap_or_else(|| {
            RowVerdict::Unknown(format!("{why}; vendor word '{word}' is undecided"))
        }),
        decided => decided.clone(),
    }
}

/// The disagreement between a decided fno verdict and the vendor's word, for
/// the caller's drift event. `None` when fno has no decided verdict, the
/// vendor said nothing recognizable, or the two agree in kind.
pub(crate) fn drift(fno: &RowVerdict, vendor: Option<&str>) -> Option<String> {
    let word = vendor.map(str::trim).filter(|w| !w.is_empty())?;
    if matches!(fno, RowVerdict::Unknown(_)) {
        return None;
    }
    let vendor_v = vendor_verdict(word)?;
    if same_kind(fno, &vendor_v) {
        return None;
    }
    Some(format!(
        "fno says {fno:?}; vendor word '{word}' says {vendor_v:?}"
    ))
}

fn same_kind(a: &RowVerdict, b: &RowVerdict) -> bool {
    matches!(
        (a, b),
        (RowVerdict::Live(_), RowVerdict::Live(_))
            | (RowVerdict::Finished(_), RowVerdict::Finished(_))
    )
}

/// The vendor words this door recognizes, mirroring the roster's own
/// terminal predicate (`done | stopped | failed`) plus the spellings the
/// other lanes emit. Anything else is undecided, never a verdict.
fn vendor_verdict(word: &str) -> Option<RowVerdict> {
    const FINISHED: [&str; 6] = ["done", "stopped", "failed", "exited", "completed", "dead"];
    const LIVE: [&str; 3] = ["working", "running", "idle"];
    if FINISHED.contains(&word) {
        Some(RowVerdict::Finished(format!("vendor word '{word}'")))
    } else if LIVE.contains(&word) {
        Some(RowVerdict::Live("vendor"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> RegistryEntry {
        RegistryEntry::new(
            Some("sid-verdict".into()),
            crate::state::Lineage::unproven("row_verdict test"),
        )
    }

    fn working_leg(
        ttl_ms: Option<u64>,
        received_at: &str,
    ) -> Option<crate::state::InsideLegReport> {
        Some(crate::state::InsideLegReport {
            state: InsideLegState::Working,
            seq: 1,
            reason: None,
            received_at: received_at.to_string(),
            ttl_ms,
        })
    }

    #[test]
    fn ac1_working_inside_leg_inside_its_ttl_is_live_with_an_unread_vendor() {
        let mut e = entry();
        e.inside_leg = working_leg(Some(90_000), &crate::daemon::now_rfc3339_like());
        assert_eq!(fno_verdict(&e), RowVerdict::Live("inside_leg"));
        // The unread vendor listing resolves to nothing and flips nothing.
        assert_eq!(
            reconcile(&fno_verdict(&e), None),
            RowVerdict::Live("inside_leg")
        );
    }

    #[test]
    fn ac2_a_row_with_no_pid_no_report_and_no_outcome_is_unknown_never_finished() {
        assert_eq!(
            fno_verdict(&entry()),
            RowVerdict::Unknown(
                "no terminal status, no live inside-leg report, no recorded pid".into()
            )
        );
    }

    #[test]
    fn a_terminal_registry_status_is_finished_despite_a_live_vendor_word() {
        let mut e = entry();
        e.status = crate::AgentStatus::Exited;
        let v = fno_verdict(&e);
        assert!(matches!(v, RowVerdict::Finished(_)));
        assert_eq!(reconcile(&v, Some("working")), v);
    }

    #[test]
    fn a_pid_proven_gone_finishes_and_a_live_pid_holds_the_row_live() {
        let mut gone = entry();
        gone.pid = Some(spawn_and_reap_pid());
        assert!(matches!(fno_verdict(&gone), RowVerdict::Finished(_)));

        let mut live = entry();
        live.pid = Some(std::process::id());
        assert_eq!(fno_verdict(&live), RowVerdict::Live("pid"));
    }

    /// Spawn a child and WAIT it: an unreaped zombie still answers kill(2),
    /// so only a reaped pid is provably ESRCH.
    fn spawn_and_reap_pid() -> u32 {
        let mut child = std::process::Command::new("/usr/bin/true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        child.wait().expect("reap true");
        pid
    }

    #[test]
    fn an_expired_working_report_does_not_hold_the_row_live() {
        let mut e = entry();
        e.inside_leg = working_leg(Some(90_000), "2020-01-01T00:00:00Z");
        assert!(matches!(fno_verdict(&e), RowVerdict::Unknown(_)));
    }

    #[test]
    fn a_vendor_word_resolves_only_an_unknown_fno_verdict() {
        let unknown = fno_verdict(&entry());
        assert_eq!(
            reconcile(&unknown, Some("done")),
            RowVerdict::Finished("vendor word 'done'".into())
        );
        assert_eq!(
            reconcile(&unknown, Some("working")),
            RowVerdict::Live("vendor")
        );
        assert!(matches!(
            reconcile(&unknown, Some("mystery")),
            RowVerdict::Unknown(_)
        ));

        let decided = RowVerdict::Live("pid");
        assert_eq!(reconcile(&decided, Some("done")), decided);
    }

    #[test]
    fn drift_names_a_disagreement_and_stays_silent_on_agreement_or_unknown() {
        let live = RowVerdict::Live("inside_leg");
        assert!(drift(&live, Some("done")).is_some());
        assert!(drift(&live, Some("working")).is_none());
        assert!(drift(&live, None).is_none());
        assert!(drift(&fno_verdict(&entry()), Some("done")).is_none());
    }

    #[test]
    fn the_door_never_branches_on_the_harness_name() {
        let mut a = entry();
        a.harness = Some("claude".into());
        let mut b = entry();
        b.harness = Some("codex".into());
        assert_eq!(fno_verdict(&a), fno_verdict(&b));
    }
}
