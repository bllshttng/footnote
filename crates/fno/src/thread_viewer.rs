use std::collections::BTreeMap;

use crate::agents_view::RegistryAgent;
use crate::tree::TabId;

/// One open portal under the TV model: the channel it is tuned to, the
/// screen pane playing it, and the tab that holds the screen's leaf. A
/// portal is never a pane and never a tab; the pane is only what plays
/// now. `row_key` is the channel: the attach id (claude) or the registry
/// name (every other harness - the command's `id` field); it holds a key a
/// row answered, or - when a claude seat's screen names no claimable row -
/// the screen's title marked unclaimed (`vellum?`), which no row answers,
/// so [`row_for_pane`] resolves no row there.
#[derive(Clone)]
pub(crate) struct Portal {
    pub(crate) row_key: String,
    pub(crate) seat: u64,
    pub(crate) tab: TabId,
}

pub(crate) fn row_for_pane<'a>(
    portals: &BTreeMap<u8, Portal>,
    pane: u64,
    agents: &'a [RegistryAgent],
) -> Option<&'a RegistryAgent> {
    let key = portals
        .values()
        .find(|portal| portal.seat == pane)?
        .row_key
        .as_str();
    let mut matches = agents.iter().filter(|row| {
        row.mux.is_none()
            && !row.exited
            && (row.name == key
                || row.effective_identity() == Some(key)
                || row.attach_id.as_deref() == Some(key))
    });
    let row = matches.next()?;
    matches.next().is_none().then_some(row)
}

pub(crate) fn identity_for_pane(
    portals: &BTreeMap<u8, Portal>,
    pane: u64,
    agents: &[RegistryAgent],
) -> Option<String> {
    row_for_pane(portals, pane, agents)
        .and_then(|row| row.effective_identity())
        .map(str::to_string)
}
