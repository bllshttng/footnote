//! Which display row a stop/remove confirm paints on: the captured sid
//! first, then the pane; a name two rows answer is ambiguous and anchors
//! nowhere. Lives outside client.rs under the file-budget gate.

use super::*;

impl View {
    /// The display-row index the confirm's target CURRENTLY occupies,
    /// matched by the identity carried in the [`ConfirmAction`] (squad id, agent
    /// name, or external/dismiss attach_id, or a card node) - never a captured
    /// numeric index. A global confirm (reap / clear-dead) has no row, and a
    /// target that vanished returns `None`; both fall back to the bottom row.
    /// A sid-carrying capture matches the sid first (the v67 identity), a
    /// pane-keyed one its pane, and a name matching more than one row is
    /// ambiguous and anchors nowhere (the mail_envelope.rs refusal): bottom
    /// row, never a wrong-row paint.
    pub(crate) fn confirm_target_index(&self, action: &ConfirmAction) -> Option<usize> {
        let hits: Vec<usize> = self
            .display_rows()
            .iter()
            .enumerate()
            .filter_map(|(i, r)| {
                let hit = match (&action.action, r) {
                    (ConfirmKind::RemoveSquad { squad, .. }, DisplayRow::Sel(s)) => {
                        s.tab.is_none() && s.squad == *squad
                    }
                    (
                        ConfirmKind::StopAgent {
                            name, sid, pane_id, ..
                        }
                        | ConfirmKind::RemoveAgent {
                            name, sid, pane_id, ..
                        },
                        DisplayRow::Agent(a),
                    ) => {
                        // The sid is the row's identity; a sid-carrying capture
                        // never falls through to name matching.
                        if let Some(cap) = sid.as_deref() {
                            // A sid-carrying capture matches only sid-bearing
                            // rows: a sidless row is never proven to be the
                            // target, so no pane or name fallthrough.
                            a.harness_session_id.as_deref() == Some(cap)
                        } else {
                            match (pane_id, a.pane_id) {
                                // No sid on one side: the pane is the next
                                // identity, and a pane-keyed capture never
                                // falls through to the label either.
                                (Some(cap), Some(pane)) => *cap == pane,
                                // A capture with no identity keys can only
                                // name the row, and a name two rows answer is
                                // ambiguous: anchor nowhere, never first-match.
                                (None, _) => a.name == *name,
                                (Some(_), None) => false,
                            }
                        }
                    }
                    (
                        ConfirmKind::StopExternal { attach_id, .. }
                        | ConfirmKind::RemoveExternal { attach_id, .. }
                        | ConfirmKind::DismissMember { attach_id, .. },
                        DisplayRow::Agent(a),
                    ) => a.attach_id.as_deref() == Some(attach_id.as_str()),
                    _ => false,
                };
                hit.then_some(i)
            })
            .collect();
        match hits.as_slice() {
            [only] => Some(*only),
            _ => None,
        }
    }
}
