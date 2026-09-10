//! The `fno mux doctor` checks over the squads store's location: the
//! migration-overlap divergence and the dead agents-home store.

use super::*;

/// The migration-overlap window: a pre-upgrade daemon still bound to the
/// legacy root keeps writing the legacy squads.json AFTER this process
/// seeded the state-root copy, and the read path stops seeing the legacy
/// file the moment the primary exists (the fallback is NotFound-only).
/// Merging the two stores is reconciliation work beyond a check, but the
/// DIVERGENCE is visible: warn when the legacy file is newer than the copy
/// it seeded, which is exactly the old server's signature.
pub(super) fn legacy_squads_newer_check() -> Check {
    if !proto::legacy_fallback_allowed() {
        return Check {
            name: "legacy squads store".into(),
            verdict: Verdict::Na,
            detail: "an explicit override isolates the store on purpose".into(),
            remedy: None,
        };
    }
    let primary = crate::squad_store::squads_path();
    let legacy = proto::legacy_sidecar_path("squads.json");
    let newer = match (std::fs::metadata(&primary), std::fs::metadata(&legacy)) {
        (Ok(p), Ok(l)) => match (p.modified(), l.modified()) {
            (Ok(pm), Ok(lm)) => lm > pm,
            _ => false,
        },
        _ => false,
    };
    if !newer {
        return Check {
            name: "legacy squads store".into(),
            verdict: Verdict::Na,
            detail: "no legacy copy is outrunning the resolved store".into(),
            remedy: None,
        };
    }
    Check {
        name: "legacy squads store".into(),
        verdict: Verdict::Warn,
        detail: format!(
            "the legacy {} is newer than the resolved {}; a pre-upgrade \
             server may still be writing it",
            legacy.display(),
            primary.display()
        ),
        remedy: Some(
            "restart the old mux server under the resolved root, then run \
             `fno mux workspace prune` once"
                .into(),
        ),
    }
}

/// A dead squads store at the agents-home path (`~/.fno/agents/squads.json`)

/// A dead squads store at the agents-home path (`~/.fno/agents/squads.json`)
/// reads as authoritative precisely because it sits beside live agent files
/// (registry.json IS authoritative there), and it produced one false report
/// before anyone noticed. No code reads that spelling, so the file can only
/// mislead: name it and its live replacement here, which is the one place a
/// reader asking "which store is real" already looks.
pub(super) fn agents_squads_orphan_check() -> Check {
    if !proto::legacy_fallback_allowed() {
        return Check {
            name: "agents-home squads store".into(),
            verdict: Verdict::Na,
            detail: "an explicit override isolates the store on purpose".into(),
            remedy: None,
        };
    }
    let orphan = proto::legacy_agents_home().join("squads.json");
    let primary = crate::squad_store::squads_path();
    let same_file = match (
        std::fs::canonicalize(&orphan),
        std::fs::canonicalize(&primary),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => orphan == primary,
    };
    if same_file {
        return Check {
            name: "agents-home squads store".into(),
            verdict: Verdict::Na,
            detail: "the agents-home path is the resolved store".into(),
            remedy: None,
        };
    }
    if !orphan.exists() {
        return Check {
            name: "agents-home squads store".into(),
            verdict: Verdict::Na,
            detail: "no store at the agents-home path".into(),
            remedy: None,
        };
    }
    Check {
        name: "agents-home squads store".into(),
        verdict: Verdict::Warn,
        detail: format!(
            "{} sits beside the live agent files, but no code reads it; the \
             live squad store is {}",
            orphan.display(),
            primary.display()
        ),
        remedy: Some(format!("rm {}", orphan.display())),
    }
}
