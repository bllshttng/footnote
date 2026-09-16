//! The spawn-time parent edge, Rust side: what a CLIENT-side mint stamps on
//! its registry row. Mirrors `cli/src/fno/agents/spawn_lineage.py`. A row
//! never says nothing: a capture that resolved no parent session carries the
//! identity disposition as its `lineage_reason`.

use crate::claims::{
    ambient_parent_edge, canonical_identity_from, CanonicalDisposition, HARNESS_SESSION_MARKERS,
    LEGACY_HARNESS_SESSION_MARKERS,
};
use crate::state::Lineage;

/// The ambient parent edge as one Lineage. A capture that resolved no
/// session carries the ambient disposition as its reason. CLIENT-side only:
/// the daemon's env is scrubbed, so a daemon mint reads the request instead
/// (`Lineage::from_request`).
pub fn ambient_lineage() -> Lineage {
    let mut lineage = Lineage::captured(ambient_parent_edge());
    if lineage.session.is_none() {
        lineage.reason = Some(ambient_lineage_reason());
    }
    lineage
}

/// The reason text an ambient capture with no parent session carries,
/// mirroring Python's `spawn_lineage._lineage_reason`: the identity's
/// disposition plus the markers it saw, so a null parent says why.
fn ambient_lineage_reason() -> String {
    let word = match canonical_identity_from(|k| std::env::var(k).ok()).2 {
        CanonicalDisposition::Absent => "absent",
        CanonicalDisposition::Invalid => "invalid",
        CanonicalDisposition::NameOnly => "name_only",
        CanonicalDisposition::Complete => "complete",
    };
    let markers: Vec<&str> = HARNESS_SESSION_MARKERS
        .iter()
        .chain(LEGACY_HARNESS_SESSION_MARKERS.iter())
        .map(|(marker, _harness)| *marker)
        .filter(|marker| std::env::var(marker).is_ok_and(|v| !v.trim().is_empty()))
        .collect();
    format!(
        "identity disposition={word}, markers={}",
        if markers.is_empty() {
            "no markers".to_string()
        } else {
            markers.join(",")
        }
    )
}
