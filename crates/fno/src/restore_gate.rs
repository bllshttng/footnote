//! The restore gate: what workspace restore must refuse, and where it reads
//! its registry rows. Restoring a worker the fleet deliberately retired
//! resurrects a dead session, so the receipts store answers "is this native
//! session id already retired" before any member resumes.

use crate::agents_view::{self, RegistryAgent};

// The registry rows the restore verb classifies against, read fresh from
// the registry file at verb time. In tests, `RESTORE_REGISTRY_ROWS`
// overrides the file (a unit test cannot populate the real registry, and
// reading it would clobber the fake rows the test installed).
#[cfg(test)]
thread_local! {
    pub(crate) static RESTORE_REGISTRY_ROWS: std::cell::RefCell<Option<Option<Vec<RegistryAgent>>>> =
        const { std::cell::RefCell::new(None) };
}

pub(crate) fn restore_registry_rows() -> Option<Vec<RegistryAgent>> {
    #[cfg(test)]
    if let Some(rows) = RESTORE_REGISTRY_ROWS.with(|p| p.borrow().clone()) {
        return rows;
    }
    std::fs::read_to_string(agents_view::registry_path())
        .ok()
        .and_then(|raw| agents_view::derive_rows(&raw, 0))
}

#[cfg(test)]
pub(crate) fn set_restore_registry_rows(rows: Vec<RegistryAgent>) {
    RESTORE_REGISTRY_ROWS.with(|p| *p.borrow_mut() = Some(Some(rows)));
}

#[cfg(test)]
pub(crate) struct RestoreRegistryRowsGuard;

#[cfg(test)]
impl Drop for RestoreRegistryRowsGuard {
    fn drop(&mut self) {
        RESTORE_REGISTRY_ROWS.with(|p| *p.borrow_mut() = None);
    }
}

// (x-8b51) The attach-ids the restore walk may treat as POSITIVELY dead
// (a claimed-live status their own recorded pid falsifies, claude rows).
// In tests, `RESTORE_STALE_IDS` overrides the file, same posture as
// `RESTORE_REGISTRY_ROWS`: a unit test cannot populate the real registry,
// and reading it would race whatever live rows the machine holds.
#[cfg(test)]
thread_local! {
    pub(crate) static RESTORE_STALE_IDS: std::cell::RefCell<Option<std::collections::HashSet<String>>> =
        const { std::cell::RefCell::new(None) };
}

pub(crate) fn stale_live_attach_ids_for_restore() -> std::collections::HashSet<String> {
    #[cfg(test)]
    if let Some(set) = RESTORE_STALE_IDS.with(|p| p.borrow().clone()) {
        return set;
    }
    std::fs::read_to_string(agents_view::registry_path())
        .ok()
        .map(|raw| agents_view::stale_live_attach_ids(&raw))
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) fn set_restore_stale_ids(ids: &[&str]) {
    RESTORE_STALE_IDS.with(|p| {
        *p.borrow_mut() = Some(ids.iter().map(|s| s.to_string()).collect());
    });
}

#[cfg(test)]
pub(crate) struct RestoreStaleIdsGuard;

#[cfg(test)]
impl Drop for RestoreStaleIdsGuard {
    fn drop(&mut self) {
        RESTORE_STALE_IDS.with(|p| p.borrow_mut().take());
    }
}

/// The native session ids a reap receipt preserves: a retired session's
/// squad membership can outlive its tombstone until the sweep's squad
/// wiring lands, so restore consults the receipts store as the death
/// record before resuming a member. `None` answers "present but
/// unreadable": like the other member-evidence readers, it contributes no
/// verdict and the caller keeps the member (the unknown-liveness
/// convention) rather than bricking restore on a broken store. A missing
/// directory is the complete-empty case: no receipt was ever written.
pub(crate) fn retired_receipt_session_ids() -> Option<std::collections::HashSet<String>> {
    let root = agents_view::registry_path()
        .parent()?
        .to_path_buf()
        .join("reap-receipts");
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Some(std::collections::HashSet::new())
        }
        Err(_) => return None,
    };
    let mut out = std::collections::HashSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        if let Some(sid) = value
            .get("harness_session_id")
            .and_then(serde_json::Value::as_str)
        {
            if !sid.is_empty() {
                out.insert(sid.to_string());
            }
        }
    }
    Some(out)
}

/// The restore refusal for one member's native session id, when a reap
/// receipt preserves it. The death record outranks every liveness guess: a
/// session the fleet retired is never a restore candidate, whatever the
/// stores say about its pane.
pub(crate) fn retired_refusal(
    session_id: Option<&str>,
    retired: &std::collections::HashSet<String>,
) -> Option<String> {
    if session_id.is_some_and(|sid| retired.contains(sid)) {
        Some(
            "retired: a reap receipt preserves this session; it is no longer a restore candidate"
                .into(),
        )
    } else {
        None
    }
}
