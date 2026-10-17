//! Who watches which pane.
//!
//! A passive client (`Attach { rows: 0, cols: 0 }`) may tag itself with
//! [`ObserverHello`]: a name, the pane it watches, and how it got here. The
//! tag lives as long as the connection. There is no heartbeat and no expiry,
//! because a closed socket already runs `CoreMsg::Gone`, and `Gone` pushes a
//! fresh `Layout` that no longer lists the observer.
//!
//! The tag is display and routing only. It never grants a passive client any
//! write path: the mutation gate in `handle_msg` still drops every
//! pty-touching message from it.

use std::collections::BTreeMap;

use super::{Core, Flow, Session, TabId};
use crate::proto::{ObserverHello, ObserverRow, ObserverVia};

/// Tags the server holds at once. A scan that opens ticket after ticket
/// cannot grow the list without bound.
const MAX_OBSERVERS: usize = 64;
/// Longest display name, after filtering.
const MAX_NAME: usize = 40;

/// A display name reduced to `[A-Za-z0-9 ._-]`, trimmed, at most 40 chars.
/// It is a label the driver reads, never an identity.
pub(crate) fn clean_label(name: &str) -> String {
    let kept: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '_' | '-'))
        .take(MAX_NAME)
        .collect();
    kept.trim().to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ObserverTag {
    name: String,
    pane: u64,
    via: ObserverVia,
    frames: bool,
}

/// The tags, keyed by the client id that sent them.
#[derive(Debug, Default)]
pub(crate) struct Tags(BTreeMap<u64, ObserverTag>);

/// The squad and tab an observer with no matching working directory anchors
/// to: the first squad's most-recently-active tab, or a `(0, 0)` sentinel when
/// the session has no squads yet. `TabId` 0 is never minted, so the sentinel is
/// a safe dangling view and the browser shows its "no panes" placeholder.
pub(super) fn passive_anchor(session: &Session) -> (u64, TabId) {
    match session.squads.first() {
        Some(sq) => {
            let tid = sq
                .tabs
                .get(sq.active_tab)
                .or_else(|| sq.tabs.first())
                .expect("a squad always has a tab")
                .id;
            (sq.id, tid)
        }
        None => (0, 0),
    }
}

/// The panes whose frames client `client` is sent. An untagged observer (the
/// web bridge) sees every pane. A tagged one sees its own pane, or none when
/// it is a presence-only ticket.
pub(super) fn frame_ids(tags: &Tags, client: u64, all: &[u64]) -> Vec<u64> {
    match tags.0.get(&client) {
        Some(t) if t.frames => all.iter().copied().filter(|p| *p == t.pane).collect(),
        Some(_) => Vec::new(),
        None => all.to_vec(),
    }
}

impl Core {
    /// Tag passive client `id`. A non-passive sender, a second tag from the
    /// same client, or a tag past the cap changes nothing.
    pub(super) fn observe(&mut self, id: u64, hello: ObserverHello) -> Flow {
        if !self.is_passive(id) {
            eprintln!("fno mux: client {id} sent Observe but is not an observer; ignored");
            return Flow::Continue;
        }
        let live: Vec<u64> = self.clients.iter().map(|c| c.id).collect();
        self.observers.0.retain(|cid, _| live.contains(cid));
        if self.observers.0.contains_key(&id) || self.observers.0.len() >= MAX_OBSERVERS {
            return Flow::Continue;
        }
        let name = match clean_label(&hello.name) {
            n if n.is_empty() => "observer".to_string(),
            n => n,
        };
        self.observers.0.insert(
            id,
            ObserverTag {
                name,
                pane: hello.pane,
                via: hello.via,
                frames: hello.frames,
            },
        );
        self.push_layout(false);
        Flow::Continue
    }

    /// One row per tagged passive client still attached, in attach order.
    pub(super) fn observer_rows(&self) -> Vec<ObserverRow> {
        self.clients
            .iter()
            .filter(|c| c.passive)
            .filter_map(|c| self.observers.0.get(&c.id))
            .map(|t| ObserverRow {
                pane_id: t.pane,
                name: t.name.clone(),
                via: t.via,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use tokio::sync::{mpsc, Notify};

    use super::super::tests::empty_core;
    use super::super::Client;
    use super::*;
    use crate::vt::Modes;

    fn client(id: u64, passive: bool) -> Client {
        Client {
            id,
            reliable_tx: mpsc::channel(1).0,
            dirty: Arc::default(),
            notify: Arc::new(Notify::new()),
            synced_modes: Modes::default(),
            view: (1, 1),
            visible: HashSet::new(),
            dims: (0, 0),
            passive,
            last_press: None,
        }
    }

    fn hello(name: &str, pane: u64) -> ObserverHello {
        ObserverHello {
            name: name.into(),
            pane,
            via: ObserverVia::Ssh,
            frames: true,
        }
    }

    #[test]
    fn a_tagged_passive_client_is_listed_until_it_leaves() {
        let mut core = empty_core();
        core.clients.push(client(7, true));
        core.clients.push(client(8, false));

        core.observe(7, hello("teach!er", 3));
        core.observe(8, hello("sneak", 3));
        let rows = core.observer_rows();
        assert_eq!(rows.len(), 1, "the non-passive sender is ignored");
        assert_eq!((rows[0].pane_id, rows[0].name.as_str()), (3, "teacher"));

        core.clients.retain(|c| c.id != 7);
        assert!(
            core.observer_rows().is_empty(),
            "a client that left is no longer listed"
        );
    }

    #[test]
    fn frames_follow_the_tag() {
        let mut core = empty_core();
        core.clients.push(client(7, true));
        core.observe(7, hello("a", 3));
        let all = [1, 3, 5];
        assert_eq!(frame_ids(&core.observers, 7, &all), vec![3]);
        assert_eq!(frame_ids(&core.observers, 99, &all), vec![1, 3, 5]);
        core.observers.0.get_mut(&7).unwrap().frames = false;
        assert!(frame_ids(&core.observers, 7, &all).is_empty());
    }
}
