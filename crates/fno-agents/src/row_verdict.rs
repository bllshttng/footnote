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
use serde_json::Value;

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

/// The crown-vacancy read: does this row's death vacate what it holds?
/// Orphaned is the reversible resumable word (a quiet live lead settles
/// Orphaned), so the word alone never vacates - only a Finished verdict
/// does. A Live row holds what it carries, and an Unknown row holds too:
/// an undecided reader never hands a crown away.
pub(crate) fn finished(e: &RegistryEntry) -> bool {
    matches!(fno_verdict(e), RowVerdict::Finished(_))
}

/// [`finished`] over a raw registry row rendered as JSON - the
/// spawn-overlay payload shape, where rows arrive as Python `asdict`
/// output and the crown-widen caller as a slim projection. Only the
/// reversible word re-answers through the door; every other status keeps
/// the legacy word list, so this read changes exactly one contract. A row
/// whose door fields do not parse keeps the legacy answer too.
pub(crate) fn finished_json(row: &Value) -> bool {
    let word = row
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if word != "orphaned" {
        return crate::announce::TERMINAL_STATUSES.contains(&word);
    }
    // Parse only the fields the door reads, never the whole row: a
    // projected caller row carries no created_at to satisfy the struct.
    let entry = serde_json::from_value::<RegistryEntry>(serde_json::json!({
        "name": row.get("name").and_then(Value::as_str).unwrap_or_default(),
        "status": word,
        "created_at": row.get("created_at").and_then(Value::as_str).unwrap_or_default(),
        "pid": row.get("pid").cloned().unwrap_or(Value::Null),
        "inside_leg": row.get("inside_leg").cloned().unwrap_or(Value::Null),
    }));
    match entry {
        Ok(entry) => finished(&entry),
        Err(_) => true,
    }
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
    const LIVE: [&str; 10] = [
        "working",
        "running",
        "idle",
        "busy",
        "blocked",
        "needs input",
        "ready",
        "live",
        "spawning",
        "restarting",
    ];
    if FINISHED.contains(&word) {
        Some(RowVerdict::Finished(format!("vendor word '{word}'")))
    } else if LIVE.contains(&word) {
        Some(RowVerdict::Live("vendor"))
    } else {
        None
    }
}

/// Spawn a child and WAIT it: an unreaped zombie still answers kill(2),
/// so only a reaped pid is provably ESRCH. One shared test helper for
/// every reader that needs a provably-dead pid.
#[cfg(test)]
pub(crate) fn reaped_pid() -> u32 {
    let mut child = std::process::Command::new("/usr/bin/true")
        .spawn()
        .expect("spawn true");
    let pid = child.id();
    child.wait().expect("reap true");
    pid
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
            posture: None,
        })
    }

    #[test]
    fn ac1_ac2_the_door_walks_evidence_order_and_the_vendor_fold() {
        // AC1: a working inside-leg report inside its TTL is Live, and an
        // unread vendor listing resolves to nothing and flips nothing.
        let mut e = entry();
        e.inside_leg = working_leg(Some(90_000), &crate::daemon::now_rfc3339_like());
        assert_eq!(fno_verdict(&e), RowVerdict::Live("inside_leg"));
        assert_eq!(
            reconcile(&fno_verdict(&e), None),
            RowVerdict::Live("inside_leg")
        );

        // AC2: no pid, no report and no outcome is Unknown, never Finished,
        // and the door never branches on the harness name.
        let mut a = entry();
        a.harness = Some("claude".into());
        let mut b = entry();
        b.harness = Some("codex".into());
        for e in [&a, &b] {
            assert_eq!(
                fno_verdict(e),
                RowVerdict::Unknown(
                    "no terminal status, no live inside-leg report, no recorded pid".into()
                )
            );
        }

        // The ladder below the TTL rung: terminal status finishes, an
        // expired report decides nothing, a pid proven gone finishes, a
        // live pid holds the row live.
        let mut e = entry();
        e.status = crate::AgentStatus::Exited;
        assert!(matches!(fno_verdict(&e), RowVerdict::Finished(_)));

        let mut e = entry();
        e.inside_leg = working_leg(Some(90_000), "2020-01-01T00:00:00Z");
        assert!(matches!(fno_verdict(&e), RowVerdict::Unknown(_)));

        let mut gone = entry();
        gone.pid = Some(reaped_pid());
        assert!(matches!(fno_verdict(&gone), RowVerdict::Finished(_)));

        let mut live = entry();
        live.pid = Some(std::process::id());
        assert_eq!(fno_verdict(&live), RowVerdict::Live("pid"));

        // The vendor fold: a recognized word resolves only an Unknown, a
        // decided fno verdict wins, and drift names the disagreement while
        // staying silent on agreement, silence, and undecided rows.
        let unknown = fno_verdict(&entry());
        assert_eq!(
            reconcile(&unknown, Some("done")),
            RowVerdict::Finished("vendor word 'done'".into())
        );
        assert_eq!(
            reconcile(&unknown, Some("working")),
            RowVerdict::Live("vendor")
        );
        // The other live spellings the lanes emit stay live words.
        for word in [
            "busy",
            "blocked",
            "needs input",
            "ready",
            "live",
            "spawning",
        ] {
            assert_eq!(reconcile(&unknown, Some(word)), RowVerdict::Live("vendor"));
        }
        assert!(matches!(
            reconcile(&unknown, Some("mystery")),
            RowVerdict::Unknown(_)
        ));

        let decided = RowVerdict::Live("pid");
        assert_eq!(reconcile(&decided, Some("done")), decided);
        assert!(drift(&decided, Some("done")).is_some());
        assert!(drift(&decided, Some("working")).is_none());
        assert!(drift(&decided, None).is_none());
        assert!(drift(&unknown, Some("done")).is_none());

        // The crown-vacancy read: the reversible word never vacates on the
        // word alone. Orphaned with a live pid holds, Orphaned with a
        // reaped pid vacates, and Orphaned with no evidence at all holds -
        // an undecided reader never hands a crown away. The decided words
        // keep the legacy list.
        let mut quiet_live = entry();
        quiet_live.status = crate::AgentStatus::Orphaned;
        quiet_live.pid = Some(std::process::id());
        assert!(!finished(&quiet_live));
        assert!(!finished_json(
            &serde_json::json!({"name": "lead", "status": "orphaned", "pid": std::process::id()})
        ));

        let mut quiet_dead = entry();
        quiet_dead.status = crate::AgentStatus::Orphaned;
        quiet_dead.pid = Some(reaped_pid());
        assert!(finished(&quiet_dead));

        let mut undecidable = entry();
        undecidable.status = crate::AgentStatus::Orphaned;
        assert!(!finished(&undecidable));
        assert!(!finished_json(&serde_json::json!({
            "name": "lead", "status": "orphaned",
        })));

        for word in ["exited", "failed", "permanent_dead"] {
            assert!(finished_json(&serde_json::json!({
                "name": "lead", "status": word,
            })));
        }
        assert!(!finished_json(&serde_json::json!({
            "name": "lead", "status": "busy",
        })));
    }
}
