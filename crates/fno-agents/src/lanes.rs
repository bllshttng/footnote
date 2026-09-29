//! Lane-slot claims: the atomic concurrency cap for parallel-mode dispatch.
//!
//! Ports `cli/src/fno/claims/lanes.py` decision for decision. The cap is
//! claim ATOMICITY over `max_lanes` fixed slot keys, never a stored count
//! (Locked Decision #7): two ticks both reading `count < max` and both
//! spawning would blow past the cap. `active_lane_count` is derived
//! observability, never the gate.
//!
//! Slot keys are repo-local coordination state: `lane-slot:` is not a
//! global-id prefix, so every worktree lane of a project shares one cap.
//! The claim holder is `parallel-lane:<lane_id>`, so two lanes never collide
//! onto one slot and a re-dispatch of the same lane idempotently re-takes
//! its OWN slot instead of inflating the cap.

use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

use crate::claims::{self, AcquireOpts, AcquireOutcome, ClaimRecord};

pub const LANE_SLOT_PREFIX: &str = "lane-slot:";
pub const LANE_HOLDER_PREFIX: &str = "parallel-lane:";
/// A lane can outlive the dispatcher tick that spawned it, so slots are
/// TTL-anchored and refreshed by the owner while the lane is alive.
pub const DEFAULT_LANE_TTL_MS: i64 = 3_600_000;

fn slot_key(index: usize) -> String {
    format!("{LANE_SLOT_PREFIX}{index}")
}

pub fn lane_holder(lane_id: &str) -> String {
    format!("{LANE_HOLDER_PREFIX}{lane_id}")
}

/// The slot key this lane already holds, if any. Index-agnostic on purpose:
/// a slot at any index (including one above a shrunken cap) is reused, so a
/// lane never ends up holding two.
pub fn find_lane_slot(lane_id: &str, root: Option<&Path>) -> Option<String> {
    let holder = lane_holder(lane_id);
    let records = claims::list(Some(LANE_SLOT_PREFIX), root, false).ok()?;
    records
        .into_iter()
        .find(|claim| claim.holder == holder)
        .map(|claim| claim.key)
}

/// Atomically acquire one of `max_lanes` fixed lane slots for `lane_id`.
///
/// `Ok(None)` is a full cap: every slot is held or contended. Enforcement is
/// the atomic grab, never a pre-read count. Lane-level idempotency: a lane
/// that already holds a live slot re-takes it (refreshed) rather than
/// grabbing a second one. An explicit `None` ttl coerces to the lane default
/// - a lane slot is ALWAYS TTL-anchored, never pinned to the transient
/// acquiring process. `extra_metadata` merges under the always-present
/// `lane_id`, which wins any key collision.
pub fn acquire_lane_slot(
    max_lanes: usize,
    lane_id: &str,
    ttl_ms: Option<i64>,
    reason: Option<&str>,
    extra_metadata: Option<Map<String, Value>>,
    root: Option<&Path>,
) -> Result<Option<ClaimRecord>, String> {
    if lane_id.is_empty() {
        return Err("lane_id must be non-empty".to_string());
    }
    if max_lanes < 1 {
        return Err(format!("max_lanes must be >= 1, got {max_lanes}"));
    }
    // Coerce before the anchor choice: pid-liveness is never a lane anchor.
    let ttl_ms = Some(ttl_ms.unwrap_or(DEFAULT_LANE_TTL_MS));

    let holder = lane_holder(lane_id);
    let mut metadata = extra_metadata.unwrap_or_default();
    metadata.insert("lane_id".to_string(), Value::String(lane_id.to_string()));

    if let Some(existing) = find_lane_slot(lane_id, root) {
        return acquire_one(&existing, &holder, ttl_ms, reason, metadata, root).map(Some);
    }

    for index in 0..max_lanes {
        match acquire_one(
            &slot_key(index),
            &holder,
            ttl_ms,
            reason,
            metadata.clone(),
            root,
        ) {
            Ok(claim) => return Ok(Some(claim)),
            // Slot held or contended: try the next rather than aborting the
            // whole selection; the callers degrade to a smaller fill.
            Err(_) => continue,
        }
    }
    Ok(None) // cap full
}

fn acquire_one(
    key: &str,
    holder: &str,
    ttl_ms: Option<i64>,
    reason: Option<&str>,
    metadata: Map<String, Value>,
    root: Option<&Path>,
) -> Result<ClaimRecord, String> {
    let opts = AcquireOpts {
        ttl_ms,
        reason: reason.map(str::to_string),
        metadata: Some(metadata),
        root: root.map(PathBuf::from),
        ..Default::default()
    };
    match claims::acquire(key, holder, opts) {
        AcquireOutcome::Acquired(claim) => Ok(claim),
        AcquireOutcome::HeldByOther { holder: held, .. } => Err(format!("claim held by {held}")),
        AcquireOutcome::Error(error) => Err(error),
    }
}

/// Release the slot held by `lane_id`. Silent no-op if it holds none.
pub fn release_lane_slot(lane_id: &str, root: Option<&Path>) -> Result<(), String> {
    let Some(slot) = find_lane_slot(lane_id, root) else {
        return Ok(());
    };
    claims::release(&slot, &lane_holder(lane_id), root, None)
}

/// Re-anchor this lane's EXISTING slot to the worker's lifecycle (LD#8).
///
/// The dispatcher acquires the slot TTL-anchored to itself and fire-and-
/// forgets; the worker calls this from `target init` to re-take its OWN slot
/// anchored to the durable session pid. The re-acquire is PURE pid-liveness
/// (`ttl_ms` None): the slot frees the instant the worker dies and never
/// expires under a still-running lane. `pid` None is a no-op - reconciling
/// to an unknown pid would instantly stale the slot and free the cap under a
/// live lane, so the dispatcher's TTL slot stays as the backstop. The slot's
/// stored metadata (the lane-fill `domain` annotation) is preserved; only
/// the liveness anchor changes.
pub fn reconcile_lane_slot(
    lane_id: &str,
    pid: Option<u32>,
    root: Option<&Path>,
) -> Result<Option<ClaimRecord>, String> {
    if lane_id.is_empty() {
        return Err("lane_id must be non-empty".to_string());
    }
    let Some(pid) = pid else {
        return Ok(None);
    };
    let holder = lane_holder(lane_id);
    let Some(existing) = find_lane_slot(lane_id, root) else {
        return Ok(None);
    };
    let metadata = match claims::list(Some(LANE_SLOT_PREFIX), root, false) {
        Ok(records) => records
            .into_iter()
            .find(|claim| claim.key == existing)
            .map(|claim| claim.metadata)
            .unwrap_or_default(),
        Err(error) => return Err(error),
    };
    let mut metadata = metadata;
    metadata.insert("lane_id".to_string(), Value::String(lane_id.to_string()));
    let opts = AcquireOpts {
        ttl_ms: None, // pure pid-liveness: frees the instant the worker dies
        pid: Some(pid),
        metadata: Some(metadata),
        root: root.map(PathBuf::from),
        ..Default::default()
    };
    match claims::acquire(&existing, &holder, opts) {
        AcquireOutcome::Acquired(claim) => Ok(Some(claim)),
        AcquireOutcome::HeldByOther { holder: held, .. } => Err(format!("claim held by {held}")),
        AcquireOutcome::Error(error) => Err(error),
    }
}

/// Count live lane slots. DERIVED observability value, never the cap gate.
/// The listing answers live claims only, so a crashed lane whose TTL lapsed
/// does not count and its slot is reclaimable. Index-agnostic: a lane above
/// a caller's current cap is still a live writer, so it still counts.
pub fn active_lane_count(root: Option<&Path>) -> usize {
    claims::list(Some(LANE_SLOT_PREFIX), root, false)
        .map(|records| records.len())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sandbox(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fno-lanes-test-{name}-{}-{}",
            std::process::id(),
            crate::claims::now_ms()
        ));
        let base = dir.join(".fno");
        std::fs::create_dir_all(&base).unwrap();
        dir
    }

    fn root_arg(dir: &Path) -> Option<&Path> {
        Some(dir)
    }

    #[test]
    fn acquire_takes_distinct_slots_until_the_cap_fills() {
        let dir = sandbox("cap");
        let root = root_arg(&dir);
        let first = acquire_lane_slot(2, "lane-a", None, None, None, root)
            .unwrap()
            .unwrap();
        assert_eq!(first.key, "lane-slot:0");
        let second = acquire_lane_slot(2, "lane-b", None, None, None, root)
            .unwrap()
            .unwrap();
        assert_eq!(second.key, "lane-slot:1");
        let third = acquire_lane_slot(2, "lane-c", None, None, None, root).unwrap();
        assert!(third.is_none(), "a full cap answers None, not a third slot");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn same_lane_id_reuses_its_own_slot() {
        let dir = sandbox("idempotent");
        let root = root_arg(&dir);
        let first = acquire_lane_slot(2, "lane-a", None, None, None, root)
            .unwrap()
            .unwrap();
        let again = acquire_lane_slot(2, "lane-a", None, None, None, root)
            .unwrap()
            .unwrap();
        assert_eq!(first.key, again.key);
        assert_eq!(active_lane_count(root), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_lane_id_and_zero_cap_refuse() {
        let dir = sandbox("validate");
        let root = root_arg(&dir);
        let err = acquire_lane_slot(2, "", None, None, None, root).unwrap_err();
        assert_eq!(err, "lane_id must be non-empty");
        let err = acquire_lane_slot(0, "lane-a", None, None, None, root).unwrap_err();
        assert_eq!(err, "max_lanes must be >= 1, got 0");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn released_slot_frees_the_cap() {
        let dir = sandbox("release");
        let root = root_arg(&dir);
        acquire_lane_slot(1, "lane-a", None, None, None, root)
            .unwrap()
            .unwrap();
        assert!(acquire_lane_slot(1, "lane-b", None, None, None, root)
            .unwrap()
            .is_none());
        release_lane_slot("lane-a", root).unwrap();
        let took = acquire_lane_slot(1, "lane-b", None, None, None, root)
            .unwrap()
            .unwrap();
        assert_eq!(took.key, "lane-slot:0");
        release_lane_slot("lane-never-was", root).unwrap(); // silent no-op
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn metadata_merges_under_lane_id() {
        let dir = sandbox("meta");
        let root = root_arg(&dir);
        let mut extra = Map::new();
        extra.insert("lane_id".to_string(), json!("SPOILED"));
        extra.insert("domain".to_string(), json!("code"));
        let claim = acquire_lane_slot(1, "lane-a", None, None, Some(extra), root)
            .unwrap()
            .unwrap();
        let meta = claim.metadata;
        assert_eq!(meta.get("lane_id"), Some(&json!("lane-a")));
        assert_eq!(meta.get("domain"), Some(&json!("code")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reconcile_reanchors_to_pid_and_preserves_metadata() {
        let dir = sandbox("reconcile");
        let root = root_arg(&dir);
        let mut extra = Map::new();
        extra.insert("domain".to_string(), json!("docs"));
        acquire_lane_slot(1, "lane-a", None, None, Some(extra), root)
            .unwrap()
            .unwrap();
        let reanchored = reconcile_lane_slot("lane-a", Some(1234), root)
            .unwrap()
            .unwrap();
        assert_eq!(reanchored.key, "lane-slot:0");
        let meta = reanchored.metadata;
        assert_eq!(meta.get("domain"), Some(&json!("docs")));
        assert_eq!(meta.get("lane_id"), Some(&json!("lane-a")));
        // A lane holding no slot is a no-op, and so is an unknown pid.
        assert!(reconcile_lane_slot("lane-ghost", Some(1), root)
            .unwrap()
            .is_none());
        assert!(reconcile_lane_slot("lane-a", None, root).unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
