//! The persisted footprint probe answer.
//!
//! A real probe writes its parseable reading to `paths.footprint_cache_json()`;
//! readers on the status path serve that row while it is fresh instead of
//! re-paying the probe child's measurement window. Every miss degrades to
//! the live probe in [`crate::spawn_gate`].

use serde_json::Value;
use std::time::Duration;

/// Freshness window for a persisted probe answer reused on the status path:
/// two daemon ticks. The machine_sample row the same footer renders is
/// already at most one tick old, so a two-tick-old spare-pool reading stays
/// in the freshness class that line has always shown.
pub(crate) const FOOTPRINT_CACHE_FRESH: Duration = Duration::from_secs(600);

/// Persist a parseable probe answer (`paths.footprint_cache_json`) so readers
/// can serve it without re-paying the probe child's measurement window.
/// Best-effort: a failed write just leaves the next reader on the live path,
/// which writes the row again.
pub(crate) fn persist_footprint_cache(raw: &str) {
    let payload: Value = match serde_json::from_str(raw) {
        Ok(payload) => payload,
        // An unparseable reading is a fact about this run; never cache it.
        Err(_) => return,
    };
    let home = crate::paths::AgentsHome::from_env();
    let row = serde_json::json!({
        "written_at": chrono::Utc::now().to_rfc3339(),
        "payload": payload,
    });
    let _ = std::fs::write(home.footprint_cache_json(), row.to_string());
}

/// The persisted payload string when the cache row is readable and was
/// written inside `max_age`; `None` on a miss, a stale row, a future-stamped
/// row, or anything unparseable. Every failure mode degrades to the live
/// probe.
fn fresh_footprint_cache(max_age: Duration) -> Option<String> {
    let home = crate::paths::AgentsHome::from_env();
    let raw = std::fs::read_to_string(home.footprint_cache_json()).ok()?;
    let row: Value = serde_json::from_str(&raw).ok()?;
    let written = chrono::DateTime::parse_from_rfc3339(row.get("written_at")?.as_str()?).ok()?;
    let age = chrono::Utc::now()
        .signed_duration_since(written)
        .to_std()
        .ok()?;
    if age > max_age {
        return None;
    }
    let payload = row.get("payload")?;
    Some(payload.to_string())
}

/// [`crate::spawn_gate::footprint_cause_raw`] with reuse: a probe answer another process paid
/// for inside `max_age` serves the reader without spawning the probe child.
/// A miss falls through to the live probe, which refreshes the cache on its
/// way out. Test seams route to the raw path untouched.
pub(crate) fn footprint_cause_cached(max_age: Duration) -> Result<String, String> {
    #[cfg(test)]
    {
        let seamed = |var: &str| std::env::var(var).map(|v| !v.is_empty()).unwrap_or(false);
        if seamed("FNO_TEST_FOOTPRINT_PAYLOAD") || seamed("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ") {
            return crate::spawn_gate::footprint_cause_raw();
        }
    }
    match fresh_footprint_cache(max_age) {
        Some(raw) => Ok(raw),
        None => crate::spawn_gate::footprint_cause_raw(),
    }
}
