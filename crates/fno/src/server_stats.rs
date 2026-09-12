//! Server-instance telemetry answers (the scoreboard's counter read).
//!
//! The human_touch emission-failure counter lives and dies with the server
//! instance, so its answer carries the measurement window instead of posing
//! as an all-time fact. Reading never resets the counter.

use crate::mux_cli::{control_roundtrip, resolve_session, EXIT_ERROR, EXIT_OK};
use crate::proto::{self, ControlVerb, ServerMsg};

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

/// `fno mux stats [--json]`: server-instance telemetry over one control
/// roundtrip. Read-only; the counter is never reset by reading it.
pub fn cli(json: bool) -> i32 {
    let env_session = std::env::var("FNO_MUX_SESSION").ok();
    let session = resolve_session(None, env_session.as_deref());
    let sock = match proto::socket_path(&session) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno mux stats: {e}");
            return EXIT_ERROR;
        }
    };
    match control_roundtrip(&sock, &session, ControlVerb::ServerStats) {
        Ok(ServerMsg::ServerStats {
            touch_emit_failures,
            started_at,
            measured_at,
        }) => {
            if json {
                let payload = serde_json::json!({
                    "session": session,
                    "touch_emit_failures": touch_emit_failures,
                    "started_at": started_at,
                    "measured_at": measured_at,
                });
                println!("{payload}");
                EXIT_OK
            } else {
                println!(
                    "touch emission failures: {touch_emit_failures} (server instance since {started_at}; measured {measured_at})"
                );
                EXIT_OK
            }
        }
        Ok(other) => {
            eprintln!("fno mux stats: unexpected reply {other:?}");
            EXIT_ERROR
        }
        Err(e) => {
            eprintln!("fno mux stats: {e}");
            EXIT_ERROR
        }
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
