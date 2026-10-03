//! Portal open and close become events. One row on the first
//! entry of an index into the portals map (`portal_opened`) and one on its
//! removal (`portal_closed`), so an incident like the draft-loss portal can
//! be answered from the journal alone: which door opened it, and what took
//! it. A retune emits a close-and-open pair, which is the honest shape: the
//! channel moved, the window stayed.

use super::*;

/// The `portal_opened` row: an index entered the portals map. Pure so tests
/// can assert the envelope, the same contract [`super::pane_close`] holds.
pub(super) fn portal_opened_row(
    mux_session: &str,
    portal: u8,
    row_key: &str,
    seat: u64,
) -> serde_json::Value {
    serde_json::json!({
        "ts": crate::review_invocation::review_invocation_timestamp(),
        "type": "portal_opened",
        "source": "daemon",
        "data": {
            "mux_session": mux_session,
            "portal": portal,
            "row_key": row_key,
            "seat": seat,
        }
    })
}

/// The `portal_closed` row: an index left the portals map. `cause` is the
/// door's word: `operator` (a deliberate close), `viewer_died` (the death
/// path whose parked screen could not take the seat), `displaced` (an
/// ordinary attach took the seat over), `stale_seat` (a half-created pane
/// was reaped), or `retune` (a reach re-tuned the index).
pub(super) fn portal_closed_row(
    mux_session: &str,
    portal: u8,
    row_key: &str,
    cause: &str,
) -> serde_json::Value {
    serde_json::json!({
        "ts": crate::review_invocation::review_invocation_timestamp(),
        "type": "portal_closed",
        "source": "daemon",
        "data": {
            "mux_session": mux_session,
            "portal": portal,
            "row_key": row_key,
            "cause": cause,
        }
    })
}

impl Core {
    /// Insert + journal: emits `portal_opened` only when the index was
    /// absent, so a fill or a reseat of a live portal stays silent.
    pub(super) fn journal_portal_open(&mut self, idx: u8, portal: Portal) {
        let fresh = !self.portals.contains_key(&idx);
        let row_key = portal.row_key.clone();
        let seat = portal.seat;
        self.portals.insert(idx, portal);
        if fresh {
            self.append_portal_event(portal_opened_row(&self.session_name, idx, &row_key, seat));
        }
    }

    /// Remove + journal `portal_closed` with the door's cause word. A
    /// missing index removes nothing and emits nothing; the caller that
    /// needs the slot keeps it (the retune door).
    pub(super) fn journal_portal_take(&mut self, idx: u8, cause: &str) -> Option<Portal> {
        let slot = self.portals.remove(&idx);
        if let Some(portal) = &slot {
            self.append_portal_event(portal_closed_row(
                &self.session_name,
                idx,
                &portal.row_key,
                cause,
            ));
        }
        slot
    }

    /// The close half of a retune whose caller already removed the slot:
    /// emits `portal_closed` for the channel the index used to show, with
    /// no map mutation. The caller journals the open half on the success
    /// path, so a focus or a failed retune never reads as a closure.
    pub(super) fn journal_portal_channel_left(&self, idx: u8, previous_row_key: &str, cause: &str) {
        self.append_portal_event(portal_closed_row(
            &self.session_name,
            idx,
            previous_row_key,
            cause,
        ));
    }

    fn append_portal_event(&self, event: serde_json::Value) {
        if crate::pane_send_audit::append_agents_event(
            &crate::pane_send_audit::pane_send_audit_events_path(),
            &event,
        )
        .is_err()
        {
            eprintln!("fno mux: portal event emit failed");
        }
    }
}
