//! Server-instance telemetry answers (the scoreboard's counter read).
//!
//! The human_touch emission-failure counter lives and dies with the server
//! instance, so its answer carries the measurement window instead of posing
//! as an all-time fact. Reading never resets the counter.

use crate::proto::ServerMsg;

/// Current time as a `YYYY-MM-DDThh:mm:ssZ` UTC stamp (same shape the squad
/// store writes).
fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    crate::squad_store::epoch_to_iso_public(secs)
}

/// Build the [`ServerMsg::ServerStats`] answer from live core state. Fields
/// are `pub(crate)` so this reader stays beside the wire shape it owns.
pub fn answer(touch_emit_failures: u64, started_at: &str) -> ServerMsg {
    ServerMsg::ServerStats {
        touch_emit_failures,
        started_at: started_at.to_string(),
        measured_at: now_iso(),
    }
}

/// One UTC stamp, for the instance start recorded at server boot.
pub fn stamp_now() -> String {
    now_iso()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_are_utc_iso_shape() {
        let stamp = now_iso();
        assert_eq!(stamp.len(), 20);
        assert!(stamp.ends_with('Z'));
        assert_eq!(stamp.as_bytes()[10], b'T');
    }
}
